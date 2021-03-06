// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//

use kvm_bindings::{kvm_irq_routing, kvm_irq_routing_entry, KVM_IRQ_ROUTING_MSI};
use kvm_ioctls::VmFd;
use std::collections::HashMap;
use std::io;
use std::mem::size_of;
use std::sync::{Arc, Mutex};
use vm_allocator::SystemAllocator;
use vm_device::interrupt::{
    InterruptIndex, InterruptManager, InterruptSourceConfig, InterruptSourceGroup, InterruptType,
    PCI_MSI_IRQ,
};
use vmm_sys_util::eventfd::EventFd;

/// Reuse std::io::Result to simplify interoperability among crates.
pub type Result<T> = std::io::Result<T>;

// Returns a `Vec<T>` with a size in bytes at least as large as `size_in_bytes`.
fn vec_with_size_in_bytes<T: Default>(size_in_bytes: usize) -> Vec<T> {
    let rounded_size = (size_in_bytes + size_of::<T>() - 1) / size_of::<T>();
    let mut v = Vec::with_capacity(rounded_size);
    v.resize_with(rounded_size, T::default);
    v
}

// The kvm API has many structs that resemble the following `Foo` structure:
//
// ```
// #[repr(C)]
// struct Foo {
//    some_data: u32
//    entries: __IncompleteArrayField<__u32>,
// }
// ```
//
// In order to allocate such a structure, `size_of::<Foo>()` would be too small because it would not
// include any space for `entries`. To make the allocation large enough while still being aligned
// for `Foo`, a `Vec<Foo>` is created. Only the first element of `Vec<Foo>` would actually be used
// as a `Foo`. The remaining memory in the `Vec<Foo>` is for `entries`, which must be contiguous
// with `Foo`. This function is used to make the `Vec<Foo>` with enough space for `count` entries.
pub fn vec_with_array_field<T: Default, F>(count: usize) -> Vec<T> {
    let element_space = count * size_of::<F>();
    let vec_size_bytes = size_of::<T>() + element_space;
    vec_with_size_in_bytes(vec_size_bytes)
}

pub struct InterruptRoute {
    pub gsi: u32,
    pub irq_fd: EventFd,
}

impl InterruptRoute {
    pub fn new(allocator: &mut SystemAllocator) -> Result<Self> {
        let irq_fd = EventFd::new(libc::EFD_NONBLOCK)?;
        let gsi = allocator
            .allocate_gsi()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Failed allocating new GSI"))?;

        Ok(InterruptRoute { gsi, irq_fd })
    }

    pub fn enable(&self, vm: &Arc<VmFd>) -> Result<()> {
        vm.register_irqfd(&self.irq_fd, self.gsi).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed registering irq_fd: {}", e),
            )
        })
    }

    pub fn disable(&self, vm: &Arc<VmFd>) -> Result<()> {
        vm.unregister_irqfd(&self.irq_fd, self.gsi).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed unregistering irq_fd: {}", e),
            )
        })
    }
}

pub struct KvmRoutingEntry {
    kvm_route: kvm_irq_routing_entry,
    masked: bool,
}

pub struct MsiInterruptGroup {
    vm_fd: Arc<VmFd>,
    gsi_msi_routes: Arc<Mutex<HashMap<u32, KvmRoutingEntry>>>,
    irq_routes: HashMap<InterruptIndex, InterruptRoute>,
}

impl MsiInterruptGroup {
    fn new(
        vm_fd: Arc<VmFd>,
        gsi_msi_routes: Arc<Mutex<HashMap<u32, KvmRoutingEntry>>>,
        irq_routes: HashMap<InterruptIndex, InterruptRoute>,
    ) -> Self {
        MsiInterruptGroup {
            vm_fd,
            gsi_msi_routes,
            irq_routes,
        }
    }

    fn set_kvm_gsi_routes(&self) -> Result<()> {
        let gsi_msi_routes = self.gsi_msi_routes.lock().unwrap();
        let mut entry_vec: Vec<kvm_irq_routing_entry> = Vec::new();
        for (_, entry) in gsi_msi_routes.iter() {
            if entry.masked {
                continue;
            }

            entry_vec.push(entry.kvm_route);
        }

        let mut irq_routing =
            vec_with_array_field::<kvm_irq_routing, kvm_irq_routing_entry>(entry_vec.len());
        irq_routing[0].nr = entry_vec.len() as u32;
        irq_routing[0].flags = 0;

        unsafe {
            let entries: &mut [kvm_irq_routing_entry] =
                irq_routing[0].entries.as_mut_slice(entry_vec.len());
            entries.copy_from_slice(&entry_vec);
        }

        self.vm_fd.set_gsi_routing(&irq_routing[0]).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed setting GSI routing: {}", e),
            )
        })
    }

    fn mask_kvm_entry(&self, index: InterruptIndex, mask: bool) -> Result<()> {
        if let Some(route) = self.irq_routes.get(&index) {
            let mut gsi_msi_routes = self.gsi_msi_routes.lock().unwrap();
            if let Some(kvm_entry) = gsi_msi_routes.get_mut(&route.gsi) {
                kvm_entry.masked = mask;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("mask: No existing route for interrupt index {}", index),
                ));
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("mask: Invalid interrupt index {}", index),
            ));
        }

        self.set_kvm_gsi_routes()
    }
}

impl InterruptSourceGroup for MsiInterruptGroup {
    fn enable(&self) -> Result<()> {
        for (_, route) in self.irq_routes.iter() {
            route.enable(&self.vm_fd)?;
        }

        Ok(())
    }

    fn disable(&self) -> Result<()> {
        for (_, route) in self.irq_routes.iter() {
            route.disable(&self.vm_fd)?;
        }

        Ok(())
    }

    fn trigger(&self, index: InterruptIndex) -> Result<()> {
        if let Some(route) = self.irq_routes.get(&index) {
            return route.irq_fd.write(1);
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("trigger: Invalid interrupt index {}", index),
        ))
    }

    fn notifier(&self, index: InterruptIndex) -> Option<&EventFd> {
        if let Some(route) = self.irq_routes.get(&index) {
            return Some(&route.irq_fd);
        }

        None
    }

    fn update(&self, index: InterruptIndex, config: InterruptSourceConfig) -> Result<()> {
        if let Some(route) = self.irq_routes.get(&index) {
            if let InterruptSourceConfig::MsiIrq(cfg) = &config {
                let mut kvm_route = kvm_irq_routing_entry {
                    gsi: route.gsi,
                    type_: KVM_IRQ_ROUTING_MSI,
                    ..Default::default()
                };

                kvm_route.u.msi.address_lo = cfg.low_addr;
                kvm_route.u.msi.address_hi = cfg.high_addr;
                kvm_route.u.msi.data = cfg.data;

                let kvm_entry = KvmRoutingEntry {
                    kvm_route,
                    masked: false,
                };

                self.gsi_msi_routes
                    .lock()
                    .unwrap()
                    .insert(route.gsi, kvm_entry);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Interrupt config type not supported",
                ));
            }

            return self.set_kvm_gsi_routes();
        }

        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("update: Invalid interrupt index {}", index),
        ))
    }

    fn mask(&self, index: InterruptIndex) -> Result<()> {
        self.mask_kvm_entry(index, true)
    }

    fn unmask(&self, index: InterruptIndex) -> Result<()> {
        self.mask_kvm_entry(index, false)
    }
}

pub struct KvmInterruptManager {
    allocator: Arc<Mutex<SystemAllocator>>,
    vm_fd: Arc<VmFd>,
    gsi_msi_routes: Arc<Mutex<HashMap<u32, KvmRoutingEntry>>>,
}

impl KvmInterruptManager {
    pub fn new(
        allocator: Arc<Mutex<SystemAllocator>>,
        vm_fd: Arc<VmFd>,
        gsi_msi_routes: Arc<Mutex<HashMap<u32, KvmRoutingEntry>>>,
    ) -> Self {
        KvmInterruptManager {
            allocator,
            vm_fd,
            gsi_msi_routes,
        }
    }
}

impl InterruptManager for KvmInterruptManager {
    fn create_group(
        &self,
        interrupt_type: InterruptType,
        base: InterruptIndex,
        count: InterruptIndex,
    ) -> Result<Arc<Box<dyn InterruptSourceGroup>>> {
        let mut allocator = self.allocator.lock().unwrap();

        let mut irq_routes: HashMap<InterruptIndex, InterruptRoute> =
            HashMap::with_capacity(count as usize);
        for i in base..count {
            irq_routes.insert(i, InterruptRoute::new(&mut allocator)?);
        }

        let interrupt_source_group: Arc<Box<dyn InterruptSourceGroup>> = match interrupt_type {
            PCI_MSI_IRQ => Arc::new(Box::new(MsiInterruptGroup::new(
                self.vm_fd.clone(),
                self.gsi_msi_routes.clone(),
                irq_routes,
            ))),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Interrupt type not supported",
                ))
            }
        };

        Ok(interrupt_source_group)
    }

    fn destroy_group(&self, _group: Arc<Box<dyn InterruptSourceGroup>>) -> Result<()> {
        Ok(())
    }
}
