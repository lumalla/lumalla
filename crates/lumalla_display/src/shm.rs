use std::{collections::HashMap, ffi::c_void, os::fd::RawFd};

use log::warn;
use lumalla_wayland_protocol::{ClientId, ObjectId};
use nix::libc::{MAP_FAILED, MAP_SHARED, PROT_READ, mmap, munmap};

#[derive(Debug, Default)]
pub struct ShmManager {
    shm_pool_index: HashMap<(ClientId, ObjectId), usize>,
    shm_pools: Vec<ShmPool>,
    free_shm_pool_indexes: Vec<usize>,
}

impl ShmManager {
    #[must_use]
    pub fn create_pool(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        fd: RawFd,
        size: usize,
    ) -> bool {
        let mut shm_pool = ShmPool::new(fd, size);
        let map_success = !shm_pool.map();
        shm_pool.ref_count += 1;
        let index = if let Some(index) = self.free_shm_pool_indexes.pop() {
            self.shm_pools[index] = shm_pool;
            index
        } else {
            self.shm_pools.push(shm_pool);
            self.shm_pools.len() - 1
        };
        self.shm_pool_index.insert((client_id, object_id), index);
        map_success
    }

    pub fn delete_pool(&mut self, client_id: ClientId, object_id: ObjectId) {
        if let Some(index) = self.shm_pool_index.remove(&(client_id, object_id)) {
            self.reduce_ref_count(index);
        }
    }

    #[must_use]
    pub fn resize_pool(&mut self, client_id: ClientId, object_id: ObjectId, size: usize) -> bool {
        if let Some(index) = self.shm_pool_index.get(&(client_id, object_id)) {
            return self.shm_pools[*index].resize(size);
        }
        false
    }

    fn reduce_ref_count(&mut self, pool_index: usize) {
        let pool = &mut self.shm_pools[pool_index];
        pool.ref_count -= 1;
        if pool.ref_count == 0 {
            self.free_shm_pool_indexes.push(pool_index);
            pool.unmap();
        }
    }

    #[must_use]
    pub fn create_buffer(
        &mut self,
        _client_id: ClientId,
        _object_id: ObjectId,
        _buffer_id: ObjectId,
        _offset: usize,
        _width: usize,
        _height: usize,
        _stride: usize,
        _format: u32,
    ) -> bool {
        todo!()
    }
}

#[derive(Debug)]
pub struct ShmPool {
    fd: RawFd,
    size: usize,
    address: *mut c_void,
    ref_count: usize,
}

impl ShmPool {
    pub fn new(fd: RawFd, size: usize) -> Self {
        Self {
            fd,
            size,
            address: MAP_FAILED,
            ref_count: 0,
        }
    }

    #[must_use]
    pub fn map(&mut self) -> bool {
        self.address = unsafe {
            mmap(
                std::ptr::null_mut(),
                self.size,
                PROT_READ,
                MAP_SHARED,
                self.fd,
                0,
            )
        };

        self.address != MAP_FAILED
    }

    pub fn unmap(&mut self) {
        if self.address != MAP_FAILED {
            unsafe {
                munmap(self.address, self.size);
            }
            self.address = std::ptr::null_mut();
        }
    }

    #[must_use]
    pub fn resize(&mut self, size: usize) -> bool {
        if self.size >= size {
            warn!("Tried to resize shm_pool from {} to {}", self.size, size);
            return false;
        }
        self.size = size;
        self.unmap();
        self.map()
    }
}
