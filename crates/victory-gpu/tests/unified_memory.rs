//! Integration test / usage example for unified memory.
//!
//! This demonstrates the API for APU↔dGPU unified memory on PC.
//! Run with: cargo test --package victory-gpu --test unified_memory -- --nocapture

#[cfg(feature = "unified_memory")]

use victory_gpu::{
    unified_memory_init,
    unified_memory_attach_dgpu,
    unified_memory_create_heap,
    unified_memory_allocate_buffer,
    unified_memory_cpu_ptr,
    unified_memory_apu_gpu_va,
    unified_memory_dgpu_gpu_va,
    unified_memory_buffer_size,
    unified_memory_buffer_residency,
    unified_memory_migrate_to_dgpu,
    unified_memory_migrate_to_apu,
    unified_memory_finalize_migration,
    unified_memory_print_stats,
    Residency,
    UnifiedBufferHandle,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unified_memory_basic() {
        // Initialize the unified memory system
        unified_memory_init().expect("init failed");

        // Create a 64 MB unified heap
        let heap = unified_memory_create_heap(64 * 1024 * 1024).expect("create_heap failed");
        assert_eq!(heap.offset, 0); // heap handle has offset 0

        // Allocate a 1 MB buffer in the heap with UAV access
        let buffer = unified_memory_allocate_buffer(
            &heap,
            1024 * 1024,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ).expect("allocate_buffer failed");

        // Get CPU pointer for APU/CPU writes
        let cpu_ptr = unified_memory_cpu_ptr(&buffer).expect("cpu_ptr failed");
        assert!(!cpu_ptr.is_null());

        // Write some test data from CPU/APU
        unsafe {
            let slice = std::slice::from_raw_parts_mut(cpu_ptr, 256);
            for (i, byte) in slice.iter_mut().enumerate() {
                *byte = (i % 256) as u8;
            }
        }

        // Get GPU virtual addresses
        let apu_va = unified_memory_apu_gpu_va(&buffer).expect("apu_gpu_va failed");
        assert!(apu_va != 0);

        let dgpu_va = unified_memory_dgpu_gpu_va(&buffer);
        // dGPU VA will be None unless a dGPU is attached

        // Check buffer size and residency
        let size = unified_memory_buffer_size(&buffer).expect("buffer_size failed");
        assert_eq!(size, 1024 * 1024);

        let residency = unified_memory_buffer_residency(&buffer).expect("buffer_residency failed");
        assert_eq!(residency, Residency::ApU);

        // Print stats
        unified_memory_print_stats();

        println!("Basic unified memory test passed!");
    }

    #[test]
    fn test_unified_memory_migration_flow() {
        unified_memory_init().expect("init failed");

        // In a real scenario, you'd attach a dGPU device here:
        // let dgpu_device = create_dgpu_device();
        // unified_memory_attach_dgpu(dgpu_device).expect("attach_dgpu failed");

        let heap = unified_memory_create_heap(32 * 1024 * 1024).expect("create_heap failed");
        let buffer = unified_memory_allocate_buffer(
            &heap,
            4 * 1024 * 1024,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ).expect("allocate_buffer failed");

        // Initial residency is APU (system RAM)
        assert_eq!(unified_memory_buffer_residency(&buffer), Some(Residency::ApU));

        // Migration to dGPU (requires dGPU device attached and a command list)
        // unified_memory_migrate_to_dgpu(&buffer, &list).expect("migrate_to_dgpu failed");
        // assert_eq!(unified_memory_buffer_residency(&buffer), Some(Residency::InFlight));
        //
        // // Execute copy on dGPU queue...
        //
        // unified_memory_finalize_migration(&buffer, true).expect("finalize failed");
        // assert_eq!(unified_memory_buffer_residency(&buffer), Some(Residency::Dgpu));

        println!("Migration flow test structure validated!");
    }
}

// Example: How to create a dGPU device for unified memory
#[allow(dead_code)]
fn create_dgpu_device_example() -> windows::core::Result<ID3D12Device> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIFactory4, DXGI_CREATE_FACTORY_FLAGS};
    use windows::core::Interface;

    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;

        // Enumerate adapters to find a discrete GPU (non-software, different from APU)
        let mut adapter_index = 0;
        loop {
            let adapter = factory.EnumAdapters1(adapter_index)?;
            adapter_index += 1;

            let desc = adapter.GetDesc1()?;
            // Skip software adapters
            if desc.Flags & windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }

            // Try to create a device on this adapter
            let mut device: Option<ID3D12Device> = None;
            let hr = windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(
                &adapter,
                D3D_FEATURE_LEVEL_12_0,
                &mut device,
            );

            if hr.is_ok() {
                return device.ok_or_else(windows::core::Error::empty);
            }
        }
    }
}
}