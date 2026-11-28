//! Test Xbox unified memory emulation for RTX 3500 ADA 12GB
//!
//! These tests require a D3D12-compatible GPU to run. They will be skipped
//! if running in an environment without DirectX 12 hardware available.

#[cfg(all(windows, feature = "unified_memory"))]
mod tests {
    use victory_gpu::{
        xbox_unified_init,
        xbox_unified_allocate_buffer,
        xbox_unified_print_stats,
        xbox_unified_record_mip_access,
        XboxMemoryPool,
        Residency,
    };
    use windows::Win32::Graphics::Direct3D12::*;

    /// Check if we should run D3D12 tests.
    /// Tests are skipped if SKIP_D3D12_TESTS env var is set (useful for CI).
    fn should_run_d3d12_tests() -> bool {
        !std::env::var_os("SKIP_D3D12_TESTS").is_some()
    }

    #[test]
    fn test_xbox_unified_memory_init() {
        // Initialize Xbox unified memory system
        let result = xbox_unified_init();
        assert!(result.is_ok(), "Xbox unified memory init should succeed");
    }

    #[test]
    fn test_xbox_unified_allocate_gpu_optimal() {
        if !should_run_d3d12_tests() {
            eprintln!("Skipping test: no D3D12 device available (SKIP_D3D12_TESTS set)");
            return;
        }
        xbox_unified_init().unwrap();

        // Allocate a large buffer in GPU Optimal pool (VRAM)
        let size = 16 * 1024 * 1024; // 16 MB
        let handle = xbox_unified_allocate_buffer(
            size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::GpuOptimal,
        );
        assert!(handle.is_ok(), "GPU Optimal allocation should succeed");
        let handle = handle.unwrap();

        // Verify handle properties
        assert!(handle.cpu_ptr().is_none(), "GPU Optimal buffers should not have CPU mapping");
        assert!(handle.apu_gpu_va().is_some(), "Should have APU GPU VA");
        assert!(handle.dgpu_gpu_va().is_some(), "Should have dGPU GPU VA when attached");
        assert_eq!(handle.size(), Some(size.next_multiple_of(64 * 1024)), "Size should be aligned");
        assert_eq!(handle.target_pool(), Some(XboxMemoryPool::GpuOptimal), "Should be GPU Optimal pool");
        assert_eq!(handle.residency(), Some(Residency::GpuOptimal), "Should be in GPU Optimal residency");
    }

    #[test]
    fn test_xbox_unified_allocate_standard() {
        if !should_run_d3d12_tests() {
            eprintln!("Skipping test: no D3D12 device available (SKIP_D3D12_TESTS set)");
            return;
        }
        xbox_unified_init().unwrap();

        // Allocate a medium buffer in Standard pool
        let size = 1 * 1024 * 1024; // 1 MB
        let handle = xbox_unified_allocate_buffer(
            size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::Standard,
        );
        assert!(handle.is_ok(), "Standard allocation should succeed");
        let handle = handle.unwrap();

        // Verify handle properties
        assert!(handle.cpu_ptr().is_some(), "Standard buffers should have CPU mapping");
        assert!(handle.apu_gpu_va().is_some(), "Should have APU GPU VA");
        assert_eq!(handle.size(), Some(size.next_multiple_of(64 * 1024)), "Size should be aligned");
        assert_eq!(handle.target_pool(), Some(XboxMemoryPool::Standard), "Should be Standard pool");
        assert_eq!(handle.residency(), Some(Residency::Standard), "Should be in Standard residency");
    }

    #[test]
    fn test_xbox_unified_allocate_system() {
        if !should_run_d3d12_tests() {
            eprintln!("Skipping test: no D3D12 device available (SKIP_D3D12_TESTS set)");
            return;
        }
        xbox_unified_init().unwrap();

        // Allocate a small buffer in System pool
        let size = 64 * 1024; // 64 KB
        let handle = xbox_unified_allocate_buffer(
            size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::System,
        );
        assert!(handle.is_ok(), "System allocation should succeed");
        let handle = handle.unwrap();

        // Verify handle properties
        assert!(handle.cpu_ptr().is_some(), "System buffers should have CPU mapping");
        assert!(handle.apu_gpu_va().is_some(), "Should have APU GPU VA");
        assert_eq!(handle.size(), Some(size), "Size should be aligned");
        assert_eq!(handle.target_pool(), Some(XboxMemoryPool::System), "Should be System pool");
        assert_eq!(handle.residency(), Some(Residency::System), "Should be in System residency");
    }

    #[test]
    fn test_xbox_unified_migrations() {
        if !should_run_d3d12_tests() {
            eprintln!("Skipping test: no D3D12 device available (SKIP_D3D12_TESTS set)");
            return;
        }
        xbox_unified_init().unwrap();

        // Allocate in Standard pool
        let size = 4 * 1024 * 1024; // 4 MB
        let handle = xbox_unified_allocate_buffer(
            size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::Standard,
        ).unwrap();

        // Initially in Standard residency
        assert_eq!(handle.residency(), Some(Residency::Standard));

        // Note: Migration tests require a HostList which is only available in integration tests
        // These are tested separately in integration tests
    }

    #[test]
    fn test_xbox_unified_sfs() {
        if !should_run_d3d12_tests() {
            eprintln!("Skipping test: no D3D12 device available (SKIP_D3D12_TESTS set)");
            return;
        }
        xbox_unified_init().unwrap();

        // Allocate a texture buffer
        let size = 8 * 1024 * 1024; // 8 MB
        let handle = xbox_unified_allocate_buffer(
            size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::GpuOptimal,
        ).unwrap();

        // Record mip accesses (SFS emulation)
        let result = xbox_unified_record_mip_access(&handle, 0); // Base mip
        assert!(result.is_ok(), "SFS record mip access should succeed");

        let result = xbox_unified_record_mip_access(&handle, 2); // Mip 2
        assert!(result.is_ok(), "SFS record mip access should succeed");

        // Process SFS feedback
        // Note: requires HostList, tested in integration tests
    }

    #[test]
    fn test_xbox_unified_stats() {
        if !should_run_d3d12_tests() {
            eprintln!("Skipping test: no D3D12 device available (SKIP_D3D12_TESTS set)");
            return;
        }
        xbox_unified_init().unwrap();

        // Allocate some buffers
        let _h1 = xbox_unified_allocate_buffer(
            16 * 1024 * 1024,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::GpuOptimal,
        ).unwrap();

        let _h2 = xbox_unified_allocate_buffer(
            1 * 1024 * 1024,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::Standard,
        ).unwrap();

        let _h3 = xbox_unified_allocate_buffer(
            64 * 1024,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            XboxMemoryPool::System,
        ).unwrap();

        // Print stats (should not panic)
        xbox_unified_print_stats();
    }
}