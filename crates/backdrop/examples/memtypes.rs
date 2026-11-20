//! Temporary probe: which memory types exist, on which device, and how fast can
//! the CPU read them? Uses raw `ash` so we see through blade's choices.

use ash::vk;
use std::hint::black_box;
use std::time::Instant;

fn time_read(ptr: *const u8, bytes: usize, out: &mut [u8]) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..3 {
        let started = Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), bytes) };
        black_box(&out[..]);
        best = best.min(started.elapsed().as_secs_f64());
    }
    best
}

fn main() {
    let entry = unsafe { ash::Entry::load() }.expect("vulkan loader");
    let app_info = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 1, 0));
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app_info),
            None,
        )
    }
    .expect("instance");

    let devices = unsafe { instance.enumerate_physical_devices() }.expect("devices");
    let bytes = 8 * 1024 * 1024usize;
    let mut out = vec![0u8; bytes];

    for device in devices {
        let props = unsafe { instance.get_physical_device_properties(device) };
        let name = unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let memory = unsafe { instance.get_physical_device_memory_properties(device) };
        println!(
            "\n== {name} (type {:?}, vendor 0x{:04x} device 0x{:04x})",
            props.device_type, props.vendor_id, props.device_id
        );
        for index in 0..memory.memory_type_count as usize {
            let ty = memory.memory_types[index];
            println!(
                "  type {index}: flags {:?} heap {} ({} MiB)",
                ty.property_flags,
                ty.heap_index,
                memory.memory_heaps[ty.heap_index as usize].size / (1024 * 1024)
            );
        }

        // Create a device and try to allocate + map each host-visible type.
        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(device) }
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .unwrap_or(0) as u32;
        let priorities = [1.0f32];
        let queue_info =
            vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&priorities);
        let device_handle = match unsafe {
            instance.create_device(device, &vk::DeviceCreateInfo::default().queue_create_infos(std::slice::from_ref(&queue_info)), None)
        } {
            Ok(handle) => handle,
            Err(err) => {
                println!("  device create failed: {err:?}");
                continue;
            }
        };

        for index in 0..memory.memory_type_count as usize {
            let flags = memory.memory_types[index].property_flags;
            if !flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
                continue;
            }
            let buffer_info = vk::BufferCreateInfo::default()
                .size(bytes as u64)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST);
            let buffer = unsafe { device_handle.create_buffer(&buffer_info, None) }.expect("buffer");
            let requirements = unsafe { device_handle.get_buffer_memory_requirements(buffer) };
            if requirements.memory_type_bits & (1 << index) == 0 {
                unsafe { device_handle.destroy_buffer(buffer, None) };
                continue;
            }
            let allocation = unsafe {
                device_handle.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(index as u32),
                    None,
                )
            };
            let allocation = match allocation {
                Ok(allocation) => allocation,
                Err(err) => {
                    println!("  type {index}: allocate failed {err:?}");
                    unsafe { device_handle.destroy_buffer(buffer, None) };
                    continue;
                }
            };
            unsafe { device_handle.bind_buffer_memory(buffer, allocation, 0) }.expect("bind");
            let ptr = unsafe {
                device_handle.map_memory(allocation, 0, requirements.size, vk::MemoryMapFlags::empty())
            }
            .expect("map") as *const u8;
            unsafe {
                for offset in (0..bytes).step_by(8) {
                    std::ptr::write_volatile(ptr.add(offset) as *mut u64, offset as u64);
                }
            }
            let elapsed = time_read(ptr, bytes, &mut out);
            println!(
                "  type {index}: read {:.3} ms -> {:.2} GB/s  ({:?})",
                elapsed * 1e3,
                bytes as f64 / elapsed / 1e9,
                flags
            );
            unsafe {
                device_handle.unmap_memory(allocation);
                device_handle.free_memory(allocation, None);
                device_handle.destroy_buffer(buffer, None);
            }
        }

        unsafe {
            device_handle.destroy_device(None);
        }
    }

    unsafe { instance.destroy_instance(None) };
}
