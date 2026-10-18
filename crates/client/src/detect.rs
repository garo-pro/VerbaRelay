//! Hardware detection and the backend recommendation shown during setup.
//!
//! Detection runs in a child process. The recommendation works in two steps:
//! 1. by device type: an NVIDIA GPU suggests CUDA, another dedicated GPU Vulkan,
//!    and an integrated GPU alone the CPU;
//! 2. by measurement: every backend that is usable without a download is
//!    benchmarked with the tiny model, and the fastest wins. This catches cases
//!    like a weak integrated GPU that Vulkan finds but the CPU outperforms.

use verbarelay_protocol::messages::{Backend, DeviceInfo, GpuInfo};

use crate::engine::EngineDevice;

/// Static facts about this machine (without GPUs).
pub fn host_info() -> (String, String, u32, u64) {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown CPU".into());
    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "unknown".into());
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    (hostname, cpu, threads, sys.total_memory())
}

/// The description sent to the server.
pub fn device_info(devices: &[EngineDevice], backend: Backend) -> DeviceInfo {
    let (hostname, cpu_name, cpu_threads, memory_total) = host_info();
    let gpus: Vec<GpuInfo> = devices
        .iter()
        .filter(|d| d.is_gpu())
        .map(|d| GpuInfo {
            name: d.description.clone(),
            backend: d.backend.clone(),
            integrated: d.kind == "igpu",
            memory_total: d.memory_total,
        })
        .collect();
    let accelerator = match backend {
        Backend::Cpu => cpu_name.clone(),
        Backend::Cuda => best_gpu(devices, "CUDA").map_or_else(|| "NVIDIA GPU".into(), |d| d.description.clone()),
        Backend::Vulkan => best_gpu(devices, "Vulkan").map_or_else(|| "Vulkan GPU".into(), |d| d.description.clone()),
    };
    DeviceInfo {
        hostname,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        cpu_name,
        cpu_threads,
        memory_total,
        gpus,
        backend,
        accelerator,
    }
}

fn best_gpu<'a>(devices: &'a [EngineDevice], backend: &str) -> Option<&'a EngineDevice> {
    devices
        .iter()
        .filter(|d| d.is_gpu() && d.backend.eq_ignore_ascii_case(backend))
        .max_by_key(|d| (d.kind == "gpu", d.memory_total))
}

fn has_nvidia(devices: &[EngineDevice]) -> bool {
    devices.iter().any(|d| {
        d.is_gpu() && (d.backend.eq_ignore_ascii_case("CUDA") || d.description.to_ascii_uppercase().contains("NVIDIA"))
    })
}

/// One selectable backend in the setup.
#[derive(Debug, Clone)]
pub struct BackendOption {
    pub backend: Backend,
    /// Device that would do the work.
    pub device: String,
    /// The backend needs an extra download first (CUDA runtime).
    pub needs_download: bool,
    /// Measured speed with the tiny model (times real time), if measured.
    pub measured_speed: Option<f64>,
    pub recommended: bool,
}

impl BackendOption {
    /// Text for the choice list, e.g. "CUDA (NVIDIA GPU): NVIDIA GeForce RTX 4070, recommended".
    pub fn label(&self) -> String {
        let mut label = format!("{}: {}", self.backend.display_name(), self.device);
        if let Some(speed) = self.measured_speed {
            label.push_str(&format!(", {speed:.0} times real time"));
        }
        if self.needs_download {
            label.push_str(", needs a download of several hundred MB");
        }
        if self.recommended {
            label.push_str(", recommended");
        }
        label
    }
}

/// Builds the options and marks the recommendation.
/// `cuda_installed` tells whether the CUDA package is already present.
pub fn backend_options(
    devices: &[EngineDevice],
    cuda_installed: bool,
    measured: &[(Backend, f64)],
) -> Vec<BackendOption> {
    let (_, cpu_name, _, _) = host_info();
    let speed_of = |b: Backend| measured.iter().find(|(m, _)| *m == b).map(|(_, s)| *s);
    let mut options = Vec::new();

    if has_nvidia(devices) && cfg!(any(windows, target_os = "linux")) {
        let device = best_gpu(devices, "CUDA")
            .or_else(|| {
                devices
                    .iter()
                    .find(|d| d.is_gpu() && d.description.to_ascii_uppercase().contains("NVIDIA"))
            })
            .map_or_else(|| "NVIDIA GPU".into(), |d| d.description.clone());
        options.push(BackendOption {
            backend: Backend::Cuda,
            device,
            needs_download: !cuda_installed,
            measured_speed: speed_of(Backend::Cuda),
            recommended: false,
        });
    }
    if let Some(gpu) = best_gpu(devices, "Vulkan") {
        let kind = if gpu.kind == "igpu" { " (integrated)" } else { "" };
        options.push(BackendOption {
            backend: Backend::Vulkan,
            device: format!("{}{kind}", gpu.description),
            needs_download: false,
            measured_speed: speed_of(Backend::Vulkan),
            recommended: false,
        });
    }
    options.push(BackendOption {
        backend: Backend::Cpu,
        device: cpu_name,
        needs_download: false,
        measured_speed: speed_of(Backend::Cpu),
        recommended: false,
    });

    let recommended = recommend(devices, &options);
    for o in &mut options {
        o.recommended = o.backend == recommended;
    }
    options
}

fn recommend(devices: &[EngineDevice], options: &[BackendOption]) -> Backend {
    // NVIDIA hardware: CUDA is the fastest path even before it is measured.
    if options.iter().any(|o| o.backend == Backend::Cuda) {
        return Backend::Cuda;
    }
    // Otherwise trust measurements when there are any.
    if let Some(best) = options
        .iter()
        .filter_map(|o| o.measured_speed.map(|s| (o.backend, s)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        return best.0;
    }
    // Without measurements: a dedicated GPU beats the CPU, an integrated one usually does not.
    if best_gpu(devices, "Vulkan").is_some_and(|d| d.kind == "gpu") {
        return Backend::Vulkan;
    }
    Backend::Cpu
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(backend: &str, kind: &str, description: &str) -> EngineDevice {
        EngineDevice {
            index: 0,
            name: description.into(),
            description: description.into(),
            backend: backend.into(),
            kind: kind.into(),
            memory_total: 8 << 30,
            memory_free: 0,
        }
    }

    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn nvidia_recommends_cuda() {
        let devices = vec![
            dev("CPU", "cpu", "cpu"),
            dev("Vulkan", "gpu", "NVIDIA GeForce RTX 4070"),
        ];
        let options = backend_options(&devices, false, &[]);
        let rec = options.iter().find(|o| o.recommended).unwrap();
        assert_eq!(rec.backend, Backend::Cuda);
        assert!(rec.needs_download);
        assert!(rec.label().contains("recommended"));
    }

    #[test]
    fn integrated_gpu_loses_to_faster_cpu() {
        let devices = vec![
            dev("CPU", "cpu", "cpu"),
            dev("Vulkan", "igpu", "AMD Radeon(TM) Graphics"),
        ];
        // Unmeasured: CPU.
        let options = backend_options(&devices, false, &[]);
        assert_eq!(options.iter().find(|o| o.recommended).unwrap().backend, Backend::Cpu);
        // Measured: the faster one wins.
        let options = backend_options(&devices, false, &[(Backend::Cpu, 9.0), (Backend::Vulkan, 4.0)]);
        assert_eq!(options.iter().find(|o| o.recommended).unwrap().backend, Backend::Cpu);
        let options = backend_options(&devices, false, &[(Backend::Cpu, 3.0), (Backend::Vulkan, 12.0)]);
        assert_eq!(options.iter().find(|o| o.recommended).unwrap().backend, Backend::Vulkan);
    }

    #[test]
    fn dedicated_amd_gpu_recommends_vulkan() {
        let devices = vec![dev("CPU", "cpu", "cpu"), dev("Vulkan", "gpu", "AMD Radeon RX 7800 XT")];
        let options = backend_options(&devices, false, &[]);
        assert_eq!(options.iter().find(|o| o.recommended).unwrap().backend, Backend::Vulkan);
    }
}
