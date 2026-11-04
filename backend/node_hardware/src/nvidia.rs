//! Shared typed access to NVIDIA device and process observations.
//!
//! The system source delegates every vendor call to `dotcore-nvml`, while injected sources keep
//! hardware inventory and exact-process sampling testable without an NVIDIA host.

use dotcore_nvml::{
    ComputeProcessMemory, DeviceMemory, DeviceSample, ProcessAccounting, Unavailable,
};

pub(crate) trait NvidiaObservationSource: Send + Sync {
    fn device_count(&self) -> Result<u32, Unavailable>;

    fn driver_version(&self) -> Result<String, Unavailable>;

    fn cuda_driver_version(&self) -> Result<i32, Unavailable> {
        Err(Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Unsupported,
        ))
    }

    fn device_model(&self, index: u32) -> Result<String, Unavailable>;

    fn device_uuid(&self, index: u32) -> Result<String, Unavailable>;

    fn device_model_by_uuid(&self, device_uuid: &str) -> Result<String, Unavailable>;

    fn device_memory(&self, index: u32) -> Result<DeviceMemory, Unavailable>;

    fn device_memory_by_uuid(&self, device_uuid: &str) -> Result<DeviceMemory, Unavailable>;

    /// Reads every per-second signal of one device; sources without the signals report memory
    /// alone and the other signals as unsupported.
    fn device_sample_by_uuid(&self, device_uuid: &str) -> Result<DeviceSample, Unavailable> {
        let unsupported = || {
            Err(Unavailable::from_kind(
                dotcore_nvml::UnavailableKind::Unsupported,
            ))
        };
        Ok(DeviceSample {
            memory: Ok(self.device_memory_by_uuid(device_uuid)?),
            utilization_percent: unsupported(),
            temperature_celsius: unsupported(),
            power_milliwatts: unsupported(),
        })
    }

    fn device_compute_processes(
        &self,
        _index: u32,
    ) -> Result<Vec<ComputeProcessMemory>, Unavailable> {
        Err(Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Unsupported,
        ))
    }

    fn device_compute_processes_by_uuid(
        &self,
        _device_uuid: &str,
    ) -> Result<Vec<ComputeProcessMemory>, Unavailable> {
        Err(Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Unsupported,
        ))
    }

    fn device_accounting_enabled(&self, _index: u32) -> Result<bool, Unavailable> {
        Err(Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Unsupported,
        ))
    }

    fn device_process_accounting(
        &self,
        _index: u32,
        _process_id: u32,
    ) -> Result<ProcessAccounting, Unavailable> {
        Err(Unavailable::from_kind(
            dotcore_nvml::UnavailableKind::Unsupported,
        ))
    }
}

pub(crate) struct SystemNvidiaObservationSource {
    nvml: &'static dotcore_nvml::Nvml,
}

impl SystemNvidiaObservationSource {
    pub(crate) fn open() -> Result<Self, Unavailable> {
        Ok(Self {
            nvml: dotcore_nvml::open()?,
        })
    }
}

impl NvidiaObservationSource for SystemNvidiaObservationSource {
    fn device_count(&self) -> Result<u32, Unavailable> {
        dotcore_nvml::device_count(self.nvml)
    }

    fn driver_version(&self) -> Result<String, Unavailable> {
        dotcore_nvml::driver_version(self.nvml)
    }

    fn cuda_driver_version(&self) -> Result<i32, Unavailable> {
        dotcore_nvml::cuda_driver_version(self.nvml)
    }

    fn device_model(&self, index: u32) -> Result<String, Unavailable> {
        dotcore_nvml::device_model(self.nvml, index)
    }

    fn device_uuid(&self, index: u32) -> Result<String, Unavailable> {
        dotcore_nvml::device_uuid(self.nvml, index)
    }

    fn device_model_by_uuid(&self, device_uuid: &str) -> Result<String, Unavailable> {
        dotcore_nvml::device_model_by_uuid(self.nvml, device_uuid)
    }

    fn device_memory(&self, index: u32) -> Result<DeviceMemory, Unavailable> {
        dotcore_nvml::device_memory(self.nvml, index)
    }

    fn device_memory_by_uuid(&self, device_uuid: &str) -> Result<DeviceMemory, Unavailable> {
        dotcore_nvml::device_memory_by_uuid(self.nvml, device_uuid)
    }

    fn device_sample_by_uuid(&self, device_uuid: &str) -> Result<DeviceSample, Unavailable> {
        dotcore_nvml::device_sample_by_uuid(self.nvml, device_uuid)
    }

    fn device_compute_processes(
        &self,
        index: u32,
    ) -> Result<Vec<ComputeProcessMemory>, Unavailable> {
        dotcore_nvml::device_compute_processes(self.nvml, index)
    }

    fn device_compute_processes_by_uuid(
        &self,
        device_uuid: &str,
    ) -> Result<Vec<ComputeProcessMemory>, Unavailable> {
        dotcore_nvml::device_compute_processes_by_uuid(self.nvml, device_uuid)
    }

    fn device_accounting_enabled(&self, index: u32) -> Result<bool, Unavailable> {
        dotcore_nvml::device_accounting_enabled(self.nvml, index)
    }

    fn device_process_accounting(
        &self,
        index: u32,
        process_id: u32,
    ) -> Result<ProcessAccounting, Unavailable> {
        dotcore_nvml::device_process_accounting(self.nvml, index, process_id)
    }
}
