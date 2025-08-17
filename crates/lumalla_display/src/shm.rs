use std::{collections::HashMap, ffi::c_void, os::fd::RawFd};

use libc::{MAP_FAILED, MAP_SHARED, PROT_READ, mmap, munmap};
use log::warn;
use lumalla_wayland_protocol::{ClientId, ObjectId};

#[derive(Debug, Default)]
pub struct ShmManager {
    shm_pool_index: HashMap<(ClientId, ObjectId), usize>,
    shm_pools: Vec<ShmPool>,
    free_shm_pool_indexes: Vec<usize>,
    buffer_index: HashMap<(ClientId, ObjectId), usize>,
    buffers: Vec<ShmBuffer>,
    free_buffer_indexes: Vec<usize>,
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
            self.reduce_pool_ref_count(index);
        }
    }

    #[must_use]
    pub fn resize_pool(&mut self, client_id: ClientId, object_id: ObjectId, size: usize) -> bool {
        if let Some(index) = self.shm_pool_index.get(&(client_id, object_id)) {
            let shm_pool = &mut self.shm_pools[*index];
            let result = shm_pool.resize(size);
            for buffer in self
                .buffers
                .iter_mut()
                .filter(|b| b.alive)
                .filter(|b| b.shm_pool_index == *index)
            {
                buffer.rebase(shm_pool.address);
            }
            return result;
        }
        false
    }

    fn reduce_pool_ref_count(&mut self, pool_index: usize) {
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
        client_id: ClientId,
        pool_id: ObjectId,
        buffer_id: ObjectId,
        offset: usize,
        width: usize,
        height: usize,
        stride: usize,
        format: u32,
    ) -> bool {
        let Some(pool_index) = self.shm_pool_index.get(&(client_id, pool_id)) else {
            warn!("Received create_buffer request for unknown pool");
            return false;
        };
        let mut buffer = ShmBuffer {
            shm_pool_index: *pool_index,
            address: MAP_FAILED,
            offset,
            _width: width,
            _height: height,
            _stride: stride,
            _format: format,
            alive: true,
        };
        buffer.rebase(self.shm_pools[*pool_index].address);
        let index = if let Some(index) = self.free_buffer_indexes.pop() {
            self.buffers[index] = buffer;
            index
        } else {
            self.buffers.push(buffer);
            self.buffers.len() - 1
        };
        self.buffer_index.insert((client_id, buffer_id), index);

        true
    }

    pub fn delete_buffer(&mut self, client_id: ClientId, buffer_id: ObjectId) {
        if let Some(index) = self.buffer_index.remove(&(client_id, buffer_id)) {
            self.free_buffer_indexes.push(index);
            let buffer = &mut self.buffers[index];
            buffer.alive = false;
            let pool_index = buffer.shm_pool_index;
            self.reduce_pool_ref_count(pool_index);
        }
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

#[derive(Debug)]
struct ShmBuffer {
    shm_pool_index: usize,
    address: *mut c_void,
    offset: usize,
    _width: usize,
    _height: usize,
    _stride: usize,
    _format: u32,
    alive: bool,
}

impl ShmBuffer {
    fn rebase(&mut self, address: *mut c_void) {
        // TODO: Add size checks
        if address == MAP_FAILED {
            self.address = MAP_FAILED;
        } else {
            self.address = unsafe { address.add(self.offset) };
        }
    }
}
