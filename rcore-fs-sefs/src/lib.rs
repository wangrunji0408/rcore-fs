#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![feature(alloc)]

extern crate alloc;

#[cfg(feature = "sgx")]
#[macro_use]
extern crate sgx_tstd as std;

use alloc::{boxed::Box, collections::BTreeMap, string::String, sync::{Arc, Weak}, vec::Vec};
use core::any::Any;
use core::fmt::{Debug, Error, Formatter};
use core::mem::uninitialized;

use bitvec::BitVec;
//use log::*;
use spin::RwLock;

use rcore_fs::dirty::Dirty;
use rcore_fs::vfs::{self, FileSystem, FsError, INode, Timespec};

use self::dev::*;
use self::structs::*;

mod structs;
pub mod dev;

/// Helper methods for `File`
impl File {
    fn read_block(&self, id: BlockId, buf: &mut [u8]) -> DevResult<()> {
        assert!(buf.len() <= BLKSIZE);
        self.read_exact_at(buf, id * BLKSIZE)
    }
    fn write_block(&self, id: BlockId, buf: &[u8]) -> DevResult<()> {
        assert!(buf.len() <= BLKSIZE);
        self.write_all_at(buf, id * BLKSIZE)
    }
    fn read_direntry(&self, id: usize) -> DevResult<DiskEntry> {
        let mut direntry: DiskEntry = unsafe { uninitialized() };
        self.read_exact_at(direntry.as_buf_mut(), DIRENT_SIZE * id)?;
        Ok(direntry)
    }
    fn write_direntry(&self, id: usize, direntry: &DiskEntry) -> DevResult<()> {
        self.write_all_at(direntry.as_buf(), DIRENT_SIZE * id)
    }
    /// Load struct `T` from given block in device
    fn load_struct<T: AsBuf>(&self, id: BlockId) -> DevResult<T> {
        let mut s: T = unsafe { uninitialized() };
        self.read_block(id, s.as_buf_mut())?;
        Ok(s)
    }
}

/// inode for SEFS
pub struct INodeImpl {
    /// inode number
    id: INodeId,
    /// on-disk inode
    disk_inode: RwLock<Dirty<DiskINode>>,
    /// back file
    file: Box<File>,
    /// Reference to FS
    fs: Arc<SEFS>,
}

impl Debug for INodeImpl {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "INode {{ id: {}, disk: {:?} }}", self.id, self.disk_inode)
    }
}

impl INodeImpl {
    /// Only for Dir
    fn get_file_inode_and_entry_id(&self, name: &str) -> Option<(INodeId, usize)> {
        (0..self.disk_inode.read().blocks as usize)
            .map(|i| {
                let entry = self.file.read_direntry(i).unwrap();
                (entry, i)
            })
            .find(|(entry, _)| entry.name.as_ref() == name)
            .map(|(entry, id)| (entry.id as INodeId, id))
    }
    fn get_file_inode_id(&self, name: &str) -> Option<INodeId> {
        self.get_file_inode_and_entry_id(name).map(|(inode_id, _)| inode_id)
    }
    /// Init dir content. Insert 2 init entries.
    /// This do not init nlinks, please modify the nlinks in the invoker.
    fn dirent_init(&self, parent: INodeId) -> vfs::Result<()> {
        self.disk_inode.write().blocks = 2;
        // Insert entries: '.' '..'
        self.file.write_direntry(0, &DiskEntry {
            id: self.id as u32,
            name: Str256::from("."),
        })?;
        self.file.write_direntry(1, &DiskEntry {
            id: parent as u32,
            name: Str256::from(".."),
        })?;
        Ok(())
    }
    fn dirent_append(&self, entry: &DiskEntry) -> vfs::Result<()> {
        let mut inode = self.disk_inode.write();
        let total = &mut inode.blocks;
        self.file.write_direntry(*total as usize, entry)?;
        *total += 1;
        Ok(())
    }
    /// remove a page in middle of file and insert the last page here, useful for dirent remove
    /// should be only used in unlink
    fn dirent_remove(&self, id: usize) -> vfs::Result<()> {
        let total = self.disk_inode.read().blocks as usize;
        debug_assert!(id < total);
        let last_direntry = self.file.read_direntry(total - 1)?;
        if id != total - 1 {
            self.file.write_direntry(id, &last_direntry)?;
        }
        self.file.set_len((total - 1) * DIRENT_SIZE)?;
        self.disk_inode.write().blocks -= 1;
        Ok(())
    }
    fn nlinks_inc(&self) {
        self.disk_inode.write().nlinks += 1;
    }
    fn nlinks_dec(&self) {
        let mut disk_inode = self.disk_inode.write();
        assert!(disk_inode.nlinks > 0);
        disk_inode.nlinks -= 1;
    }
}

impl vfs::INode for INodeImpl {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> vfs::Result<usize> {
        if self.disk_inode.read().type_ != FileType::File {
            return Err(FsError::NotFile);
        }
        let len = self.file.read_at(buf, offset)?;
        Ok(len)
    }
    fn write_at(&self, offset: usize, buf: &[u8]) -> vfs::Result<usize> {
        if self.disk_inode.read().type_ != FileType::File {
            return Err(FsError::NotFile);
        }
        let len = self.file.write_at(buf, offset)?;
        Ok(len)
    }
    /// the size returned here is logical size(entry num for directory), not the disk space used.
    fn info(&self) -> vfs::Result<vfs::FileInfo> {
        let disk_inode = self.disk_inode.read();
        Ok(vfs::FileInfo {
            inode: self.id,
            size: match disk_inode.type_ {
                FileType::File => disk_inode.size as usize,
                FileType::Dir => disk_inode.blocks as usize,
                _ => panic!("Unknown file type"),
            },
            mode: 0o777,
            type_: vfs::FileType::from(disk_inode.type_.clone()),
            blocks: disk_inode.blocks as usize,
            atime: Timespec { sec: disk_inode.atime as i64, nsec: 0 },
            mtime: Timespec { sec: disk_inode.mtime as i64, nsec: 0 },
            ctime: Timespec { sec: disk_inode.ctime as i64, nsec: 0 },
            nlinks: disk_inode.nlinks as usize,
            uid: disk_inode.uid as usize,
            gid: disk_inode.gid as usize,
        })
    }
    fn sync(&self) -> vfs::Result<()> {
        let mut disk_inode = self.disk_inode.write();
        if disk_inode.dirty() {
            self.fs.meta_file.write_block(self.id, disk_inode.as_buf())?;
            disk_inode.sync();
        }
        Ok(())
    }
    fn resize(&self, len: usize) -> vfs::Result<()> {
        if self.disk_inode.read().type_ != FileType::File {
            return Err(FsError::NotFile);
        }
        self.file.set_len(len)?;
        self.disk_inode.write().size = len as u32;
        Ok(())
    }
    fn create(&self, name: &str, type_: vfs::FileType) -> vfs::Result<Arc<vfs::INode>> {
        let info = self.info()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir);
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved);
        }

        // Ensure the name is not exist
        if !self.get_file_inode_id(name).is_none() {
            return Err(FsError::EntryExist);
        }

        // Create new INode
        let inode = match type_ {
            vfs::FileType::File => self.fs.new_inode_file()?,
            vfs::FileType::Dir => self.fs.new_inode_dir(self.id)?,
        };

        // Write new entry
        let entry = DiskEntry {
            id: inode.id as u32,
            name: Str256::from(name),
        };
        self.dirent_append(&entry)?;
        inode.nlinks_inc();
        if type_ == vfs::FileType::Dir {
            inode.nlinks_inc(); //for .
            self.nlinks_inc();  //for ..
        }

        Ok(inode)
    }
    fn unlink(&self, name: &str) -> vfs::Result<()> {
        let info = self.info()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir)
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved)
        }
        if name == "." {
            return Err(FsError::IsDir)
        }
        if name == ".." {
            return Err(FsError::IsDir)
        }

        let (inode_id, entry_id) = self.get_file_inode_and_entry_id(name).ok_or(FsError::EntryNotFound)?;
        let inode = self.fs.get_inode(inode_id);

        let type_ = inode.disk_inode.read().type_;
        if type_ == FileType::Dir {
            // only . and ..
            assert!(inode.disk_inode.read().blocks >= 2);
            if inode.disk_inode.read().blocks > 2 {
                return Err(FsError::DirNotEmpty)
            }
        }
        inode.nlinks_dec();
        if type_ == FileType::Dir {
            inode.nlinks_dec(); //for .
            self.nlinks_dec();  //for ..
        }
        self.dirent_remove(entry_id)?;

        Ok(())
    }
    fn link(&self, name: &str, other: &Arc<INode>) -> vfs::Result<()> {
        let info = self.info()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir)
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved)
        }
        if !self.get_file_inode_id(name).is_none() {
            return Err(FsError::EntryExist);
        }
        let child = other.downcast_ref::<INodeImpl>().ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &child.fs) {
            return Err(FsError::NotSameFs);
        }
        if child.info()?.type_ == vfs::FileType::Dir {
            return Err(FsError::IsDir);
        }
        let entry = DiskEntry {
            id: child.id as u32,
            name: Str256::from(name),
        };
        self.dirent_append(&entry)?;
        child.nlinks_inc();
        Ok(())
    }
    fn rename(&self, old_name: &str, new_name: &str) -> vfs::Result<()> {
        let info = self.info()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir)
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved)
        }
        if old_name == "." {
            return Err(FsError::IsDir)
        }
        if old_name == ".." {
            return Err(FsError::IsDir)
        }

        if !self.get_file_inode_id(new_name).is_none() {
            return Err(FsError::EntryExist);
        }

        let (inode_id, entry_id) = self.get_file_inode_and_entry_id(old_name)
            .ok_or(FsError::EntryNotFound)?;

        // in place modify name
        let entry = DiskEntry {
            id: inode_id as u32,
            name: Str256::from(new_name),
        };
        self.file.write_direntry(entry_id, &entry)?;

        Ok(())
    }
    fn move_(&self, old_name: &str, target: &Arc<INode>, new_name: &str) -> vfs::Result<()> {
        let info = self.info()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir)
        }
        if info.nlinks <= 0 {
            return Err(FsError::DirRemoved)
        }
        if old_name == "." {
            return Err(FsError::IsDir)
        }
        if old_name == ".." {
            return Err(FsError::IsDir)
        }

        let dest = target.downcast_ref::<INodeImpl>().ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &dest.fs) {
            return Err(FsError::NotSameFs);
        }
        if dest.info()?.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir)
        }
        if dest.info()?.nlinks <= 0 {
            return Err(FsError::DirRemoved)
        }

        if !self.get_file_inode_id(new_name).is_none() {
            return Err(FsError::EntryExist);
        }

        let (inode_id, entry_id) = self.get_file_inode_and_entry_id(old_name).ok_or(FsError::EntryNotFound)?;
        let inode = self.fs.get_inode(inode_id);

        let entry = DiskEntry {
            id: inode_id as u32,
            name: Str256::from(new_name),
        };
        dest.dirent_append(&entry)?;
        self.dirent_remove(entry_id)?;

        if inode.info()?.type_ == vfs::FileType::Dir {
            self.nlinks_dec();
            dest.nlinks_inc();
        }

        Ok(())
    }
    fn find(&self, name: &str) -> vfs::Result<Arc<vfs::INode>> {
        let info = self.info()?;
        if info.type_ != vfs::FileType::Dir {
            return Err(FsError::NotDir)
        }
        let inode_id = self.get_file_inode_id(name).ok_or(FsError::EntryNotFound)?;
        Ok(self.fs.get_inode(inode_id))
    }
    fn get_entry(&self, id: usize) -> vfs::Result<String> {
        if self.disk_inode.read().type_ != FileType::Dir {
            return Err(FsError::NotDir)
        }
        if id >= self.disk_inode.read().blocks as usize {
            return Err(FsError::EntryNotFound)
        };
        let entry = self.file.read_direntry(id)?;
        Ok(String::from(entry.name.as_ref()))
    }
    fn fs(&self) -> Arc<vfs::FileSystem> {
        self.fs.clone()
    }
    fn as_any_ref(&self) -> &Any {
        self
    }
}

impl Drop for INodeImpl {
    /// Auto sync when drop
    fn drop(&mut self) {
        self.sync().expect("Failed to sync when dropping the SEFS Inode");
        if self.disk_inode.read().nlinks <= 0 {
            self.disk_inode.write().sync();
            self.fs.free_block(self.id);
            self.fs.device.remove(self.id).unwrap();
        }
    }
}


/// Simple Encrypt File System
pub struct SEFS {
    /// on-disk superblock
    super_block: RwLock<Dirty<SuperBlock>>,
    /// blocks in use are marked 0
    free_map: RwLock<Dirty<BitVec>>,
    /// inode list
    inodes: RwLock<BTreeMap<INodeId, Weak<INodeImpl>>>,
    /// device
    device: Box<Storage>,
    /// metadata file
    meta_file: Box<File>,
    /// Pointer to self, used by INodes
    self_ptr: Weak<SEFS>,
}

impl SEFS {
    /// Load SEFS
    pub fn open(device: Box<Storage>) -> vfs::Result<Arc<Self>> {
        let meta_file = device.open(0)?;
        let super_block = meta_file.load_struct::<SuperBlock>(BLKN_SUPER)?;
        if !super_block.check() {
            return Err(FsError::WrongFs);
        }
        let free_map = meta_file.load_struct::<[u8; BLKSIZE]>(BLKN_FREEMAP)?;

        Ok(SEFS {
            super_block: RwLock::new(Dirty::new(super_block)),
            free_map: RwLock::new(Dirty::new(BitVec::from(free_map.as_ref()))),
            inodes: RwLock::new(BTreeMap::new()),
            device,
            meta_file,
            self_ptr: Weak::default(),
        }.wrap())
    }
    /// Create a new SEFS
    pub fn create(device: Box<Storage>) -> vfs::Result<Arc<Self>> {
        let blocks = BLKBITS;

        let super_block = SuperBlock {
            magic: MAGIC,
            blocks: blocks as u32,
            unused_blocks: blocks as u32 - 3,
        };
        let free_map = {
            let mut bitset = BitVec::with_capacity(BLKBITS);
            bitset.extend(core::iter::repeat(false).take(BLKBITS));
            for i in 3..blocks {
                bitset.set(i, true);
            }
            bitset
        };
        let meta_file = device.create(0)?;
        meta_file.set_len(blocks * BLKSIZE)?;

        let sefs = SEFS {
            super_block: RwLock::new(Dirty::new_dirty(super_block)),
            free_map: RwLock::new(Dirty::new_dirty(free_map)),
            inodes: RwLock::new(BTreeMap::new()),
            device,
            meta_file,
            self_ptr: Weak::default(),
        }.wrap();

        // Init root INode
        let root = sefs._new_inode(BLKN_ROOT, Dirty::new_dirty(DiskINode::new_dir()), true);
        root.dirent_init(BLKN_ROOT)?;
        root.nlinks_inc();  //for .
        root.nlinks_inc();  //for ..(root's parent is itself)
        root.sync()?;

        Ok(sefs)
    }
    /// Wrap pure SEFS with Arc
    /// Used in constructors
    fn wrap(self) -> Arc<Self> {
        // Create a Arc, make a Weak from it, then put it into the struct.
        // It's a little tricky.
        let fs = Arc::new(self);
        let weak = Arc::downgrade(&fs);
        let ptr = Arc::into_raw(fs) as *mut Self;
        unsafe { (*ptr).self_ptr = weak; }
        unsafe { Arc::from_raw(ptr) }
    }

    /// Allocate a block, return block id
    fn alloc_block(&self) -> Option<usize> {
        let mut free_map = self.free_map.write();
        let id = free_map.alloc();
        if let Some(block_id) = id {
            let mut super_block = self.super_block.write();
            if super_block.unused_blocks == 0 {
                free_map.set(block_id, true);
                return None
            }
            super_block.unused_blocks -= 1;    // will not underflow
        }
        id
    }
    /// Free a block
    fn free_block(&self, block_id: usize) {
        let mut free_map = self.free_map.write();
        assert!(!free_map[block_id]);
        free_map.set(block_id, true);
        self.super_block.write().unused_blocks += 1;
    }

    /// Create a new INode struct, then insert it to self.inodes
    /// Private used for load or create INode
    fn _new_inode(&self, id: INodeId, disk_inode: Dirty<DiskINode>, create: bool) -> Arc<INodeImpl> {
        let inode = Arc::new(INodeImpl {
            id,
            disk_inode: RwLock::new(disk_inode),
            file: match create {
                true => self.device.create(id).unwrap(),
                false => self.device.open(id).unwrap(),
            },
            fs: self.self_ptr.upgrade().unwrap(),
        });
        self.inodes.write().insert(id, Arc::downgrade(&inode));
        inode
    }
    /// Get inode by id. Load if not in memory.
    /// ** Must ensure it's a valid INode **
    fn get_inode(&self, id: INodeId) -> Arc<INodeImpl> {
        assert!(!self.free_map.read()[id]);

        // In the BTreeSet and not weak.
        if let Some(inode) = self.inodes.read().get(&id) {
            if let Some(inode) = inode.upgrade() {
                return inode;
            }
        }
        // Load if not in set, or is weak ref.
        let disk_inode = Dirty::new(self.meta_file.load_struct::<DiskINode>(id).unwrap());
        self._new_inode(id, disk_inode, false)
    }
    /// Create a new INode file
    fn new_inode_file(&self) -> vfs::Result<Arc<INodeImpl>> {
        let id = self.alloc_block().ok_or(FsError::NoDeviceSpace)?;
        let disk_inode = Dirty::new_dirty(DiskINode::new_file());
        Ok(self._new_inode(id, disk_inode, true))
    }
    /// Create a new INode dir
    fn new_inode_dir(&self, parent: INodeId) -> vfs::Result<Arc<INodeImpl>> {
        let id = self.alloc_block().ok_or(FsError::NoDeviceSpace)?;
        let disk_inode = Dirty::new_dirty(DiskINode::new_dir());
        let inode = self._new_inode(id, disk_inode, true);
        inode.dirent_init(parent)?;
        Ok(inode)
    }
    fn flush_weak_inodes(&self) {
        let mut inodes = self.inodes.write();
        let remove_ids: Vec<_> = inodes.iter().filter(|(_, inode)| {
            inode.upgrade().is_none()
        }).map(|(&id, _)| id).collect();
        for id in remove_ids.iter() {
            inodes.remove(&id);
        }
    }
}

impl vfs::FileSystem for SEFS {
    /// Write back super block if dirty
    fn sync(&self) -> vfs::Result<()> {
        let mut super_block = self.super_block.write();
        if super_block.dirty() {
            self.meta_file.write_all_at(super_block.as_buf(), BLKSIZE * BLKN_SUPER)?;
            super_block.sync();
        }
        let mut free_map = self.free_map.write();
        if free_map.dirty() {
            self.meta_file.write_all_at(free_map.as_buf(), BLKSIZE * BLKN_FREEMAP)?;
            free_map.sync();
        }
        self.flush_weak_inodes();
        for inode in self.inodes.read().values() {
            if let Some(inode) = inode.upgrade() {
                inode.sync()?;
            }
        }
        Ok(())
    }

    fn root_inode(&self) -> Arc<vfs::INode> {
        self.get_inode(BLKN_ROOT)
    }

    fn info(&self) -> &'static vfs::FsInfo {
        static INFO: vfs::FsInfo = vfs::FsInfo {
            max_file_size: MAX_FILE_SIZE,
        };
        &INFO
    }
}

impl Drop for SEFS {
    /// Auto sync when drop
    fn drop(&mut self) {
        self.sync().expect("Failed to sync when dropping the SimpleFileSystem");
    }
}

trait BitsetAlloc {
    fn alloc(&mut self) -> Option<usize>;
}

impl BitsetAlloc for BitVec {
    fn alloc(&mut self) -> Option<usize> {
        // TODO: more efficient
        let id = (0..self.len()).find(|&i| self[i]);
        if let Some(id) = id {
            self.set(id, false);
        }
        id
    }
}

impl AsBuf for BitVec {
    fn as_buf(&self) -> &[u8] {
        self.as_ref()
    }
    fn as_buf_mut(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}

impl AsBuf for [u8; BLKSIZE] {}

impl From<FileType> for vfs::FileType {
    fn from(t: FileType) -> Self {
        match t {
            FileType::File => vfs::FileType::File,
            FileType::Dir => vfs::FileType::Dir,
            _ => panic!("unknown file type"),
        }
    }
}
