//! What a job asks for. Workflows say `runs-on: superci` for the default machine (set in the dashboard), or name
//! the machine in the label itself, in any order: `superci-8cpu`, `superci-8cpu-32gb`, `superci-arm64`,
//! `superci-100disk`, `superci-windows`, `superci-gpu` (or a GPU by name: `superci-l4`),
//! `superci-aws`, `superci-ondemand`. Nothing has to be created first;
//! the machine registers with GitHub under the exact label the job used, so only jobs asking for the same can take it.
use serde::{Deserialize, Serialize};

/// A machine as asked for. Unset parts are the default machine's, then each cloud's standard machine.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Spec {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ram_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub disk_gb: Option<u32>,
    /// "x64" or "arm64".
    #[serde(default, skip_serializing_if = "Option::is_none")] pub arch: Option<String>,
    /// "linux", "windows" or "macos".
    #[serde(default, skip_serializing_if = "Option::is_none")] pub os: Option<String>,
    /// One GPU: by name (one of `GPUS`), or "any" (each place's least costly).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub gpu: Option<String>,
    /// Only this cloud ("aws", "cloudflare", "modal").
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cloud: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub on_demand: bool,
}

/// The clouds a label may name.
pub const CLOUDS: [&str; 3] = ["aws", "cloudflare", "modal"];

/// The GPUs a label may name, least costly first.
pub const GPUS: [&str; 8] = ["t4", "l4", "a10g", "l40s", "a100", "h100", "h200", "b200"];

impl Spec {
    /// What a label asks for: `base` alone is the default machine; `base-part-part…` names one. `None`: not ours;
    /// `Some(Err)`: ours, but a part is not understood (the job is told why instead of waiting forever).
    pub fn parse(label: &str, base: &str) -> Option<Result<Spec, String>> {
        let label = label.to_ascii_lowercase();
        let base = base.to_ascii_lowercase();
        if label == base { return Some(Ok(Spec::default())) }
        let rest = label.strip_prefix(&base)?.strip_prefix('-')?;
        let mut s = Spec::default();
        for part in rest.split('-') {
            let num = |suffix: &str| part.strip_suffix(suffix).and_then(|n| n.parse::<u32>().ok()).filter(|n| *n > 0);
            if let Some(n) = num("cpu") { if n > 192 { return Some(Err(format!("{n} CPUs is more than any machine has"))) } s.cpu = Some(n) }
            else if let Some(n) = num("disk") { if n > 16_000 { return Some(Err(format!("{n} GB of disk is too much"))) } s.disk_gb = Some(n) }
            else if let Some(n) = num("gb") { if n > 1536 { return Some(Err(format!("{n} GB of memory is more than any machine has"))) } s.ram_gb = Some(n) }
            else if ["arm64", "arm"].contains(&part) { s.arch = Some("arm64".into()) }
            else if ["x64", "amd64", "x86"].contains(&part) { s.arch = Some("x64".into()) }
            else if ["macos", "mac", "osx"].contains(&part) { s.os = Some("macos".into()) }
            else if ["windows", "win"].contains(&part) { s.os = Some("windows".into()) }
            else if part == "gpu" { s.gpu = Some("any".into()) }
            else if let Some(g) = GPUS.iter().find(|g| **g == part || (**g == "a10g" && part == "a10")) { s.gpu = Some(g.to_string()) }
            else if part == "linux" { s.os = Some("linux".into()) }
            else if part == "ondemand" { s.on_demand = true }
            else if CLOUDS.contains(&part) { s.cloud = Some(part.into()) }
            else { return Some(Err(format!("“{part}” in runs-on: {label} is not understood (sizes are like 8cpu, 32gb, 100disk; also arm64, windows, gpu or a GPU like l4, ondemand, or a cloud: aws, cloudflare, modal)"))) }
        }
        Some(Ok(s))
    }

    /// This, with unset parts taken from `default`.
    pub fn or(&self, default: &Spec) -> Spec {
        Spec {
            // CPU asked for without memory: memory follows the CPU (4 GB each), not the default's.
            cpu: self.cpu.or(default.cpu), ram_gb: self.ram_gb.or(if self.cpu.is_some() { None } else { default.ram_gb }), disk_gb: self.disk_gb.or(default.disk_gb),
            // A label that names another kind of machine (Windows, a GPU, arm64) does not take the default's
            // architecture or system with it (a default of arm64 does not make `-windows` Windows on arm64).
            arch: self.arch.clone().or_else(|| default.arch.clone().filter(|_| self.os.is_none() && self.gpu.is_none())),
            os: self.os.clone().or_else(|| default.os.clone().filter(|_| self.arch.is_none() && self.gpu.is_none())),
            gpu: self.gpu.clone().or_else(|| default.gpu.clone()),
            cloud: self.cloud.clone().or_else(|| default.cloud.clone()), on_demand: self.on_demand || default.on_demand,
        }
    }

    /// The architecture: as asked, else arm64 for macOS (Apple silicon, as GitHub's macos-latest) and x64 otherwise.
    pub fn arch(&self) -> &str { self.arch.as_deref().unwrap_or(if self.os() == "macos" { "arm64" } else { "x64" }) }
    pub fn os(&self) -> &str { self.os.as_deref().unwrap_or("linux") }
    /// Memory: as asked, else 4 GB per CPU.
    pub fn ram(&self) -> Option<u32> { self.ram_gb.or(self.cpu.map(|c| c * 4)) }

    /// In a few words, for the dashboard and errors ("8 CPU, 32 GB, arm64").
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        if let Some(c) = self.cpu { parts.push(format!("{c} CPU")) }
        if let Some(r) = self.ram_gb { parts.push(format!("{r} GB")) }
        if let Some(d) = self.disk_gb { parts.push(format!("{d} GB disk")) }
        if self.arch.is_some() { parts.push(self.arch().to_string()) }
        if self.os.is_some() { parts.push(self.os().to_string()) }
        match self.gpu.as_deref() { Some("any") => parts.push("a GPU".into()), Some(g) => parts.push(format!("{} GPU", g.to_uppercase())), None => {} }
        if self.on_demand { parts.push("on-demand".into()) }
        if let Some(c) = &self.cloud { parts.push(format!("only {c}")) }
        if parts.is_empty() { "the default machine".into() } else { parts.join(", ") }
    }
}

/// What a place can run: the largest machine, which systems, its standard machine (for what a label leaves out), and
/// its memory: per CPU when only CPUs are asked for, and the least it gives per CPU.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Capacity {
    pub max_cpu: u32, pub max_ram_gb: u32, pub max_disk_gb: u32, pub arch: Vec<String>, pub os: Vec<String>, pub on_demand: bool,
    pub std_cpu: u32, pub std_ram_gb: u32, pub std_disk_gb: u32,
    /// Memory for each CPU asked for when the label names no memory (up to the most it has); 0: none promised.
    pub ram_per_cpu: u32,
    /// The least memory it gives per CPU (memory asked for below it is rounded up).
    pub min_ram_per_cpu: u32,
    /// The GPUs it has (one per machine), least costly first; none: no GPUs.
    #[serde(default)] pub gpus: Vec<String>,
}

/// A machine as a place starts it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Size { pub cpu: u32, pub ram_gb: u32, pub disk_gb: u32,
    /// Its GPU (one of `GPUS`), if it has one.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")] pub gpu: Option<&'static str> }

impl Capacity {
    /// The machine this place starts for `s`, or why it cannot.
    pub fn fit(&self, s: &Spec) -> Result<Size, String> {
        let cpu = s.cpu.unwrap_or(self.std_cpu);
        let ram = match (s.ram_gb, s.cpu) {
            (Some(r), _) => r,
            (None, Some(c)) => (c * self.ram_per_cpu).min(self.max_ram_gb),
            (None, None) => self.std_ram_gb,
        }.max(cpu * self.min_ram_per_cpu);
        let disk = s.disk_gb.unwrap_or(self.std_disk_gb);
        if cpu > self.max_cpu { return Err(format!("at most {} CPU", self.max_cpu)) }
        if ram > self.max_ram_gb { return Err(format!("at most {} GB memory", self.max_ram_gb)) }
        if disk > self.max_disk_gb { return Err(format!("at most {} GB disk", self.max_disk_gb)) }
        if !self.arch.iter().any(|a| a == s.arch()) { return Err(format!("{} only", self.arch.join(", "))) }
        if !self.os.iter().any(|o| o == s.os()) { return Err(format!("{} only", self.os.join(", "))) }
        if s.on_demand && !self.on_demand { return Err("no on-demand machines".into()) }
        if s.os() == "windows" && s.arch() != "x64" { return Err("Windows on x64 only".into()) }
        let gpu = match s.gpu.as_deref() {
            None => None,
            Some(_) if self.gpus.is_empty() => return Err("no GPUs".into()),
            Some(_) if s.os() == "windows" => return Err("no Windows machines with GPUs".into()),
            Some(_) if s.arch() != "x64" => return Err("GPUs on x64 only".into()),
            Some(g) => match self.gpus.iter().find(|m| g == "any" || *m == g) {
                Some(m) => GPUS.iter().copied().find(|k| k == m),
                None => return Err(format!("GPUs {} only", self.gpus.join(", ").to_uppercase())),
            },
        };
        Ok(Size { cpu, ram_gb: ram, disk_gb: disk, gpu })
    }

    /// Why this place cannot run `s`, if it cannot.
    pub fn refuses(&self, s: &Spec) -> Option<String> { self.fit(s).err() }

    /// AWS: any size EC2 has, x64 and arm64, Linux and Windows, spot or on-demand; standard 4 CPU, 8 GB, 60 GB disk.
    /// GPUs: NVIDIA T4 (g4dn), L4 (g6), A10G (g5), L40S (g6e), one per machine.
    pub fn aws() -> Self { Capacity { max_cpu: 192, max_ram_gb: 1536, max_disk_gb: 16_000, arch: vec!["x64".into(), "arm64".into()], os: vec!["linux".into(), "windows".into()], on_demand: true,
        std_cpu: 4, std_ram_gb: 8, std_disk_gb: 60, ram_per_cpu: 4, min_ram_per_cpu: 0, gpus: ["t4", "l4", "a10g", "l40s"].map(String::from).to_vec() } }
    /// Cloudflare containers, sized per job: 1 to 4 CPU, at least 3 GB per CPU, up to 12 GB and 20 GB disk, x64. CPU is
    /// billed only while used, so the standard machine is the largest: 4 CPU, 12 GB, 20 GB.
    pub fn cloudflare() -> Self { Capacity { max_cpu: 4, max_ram_gb: 12, max_disk_gb: 20, arch: vec!["x64".into()], os: vec!["linux".into()], on_demand: true,
        std_cpu: 4, std_ram_gb: 12, std_disk_gb: 20, ram_per_cpu: 4, min_ram_per_cpu: 3, gpus: vec![] } }
    /// Modal sandboxes (non-preemptible, x64); standard 2 CPU, 8 GB; any of Modal's GPUs, one per sandbox.
    pub fn modal() -> Self { Capacity { max_cpu: 64, max_ram_gb: 336, max_disk_gb: 512, arch: vec!["x64".into()], os: vec!["linux".into()], on_demand: true,
        std_cpu: 2, std_ram_gb: 8, std_disk_gb: 64, ram_per_cpu: 4, min_ram_per_cpu: 0, gpus: GPUS.map(String::from).to_vec() } }
}

/// EC2 instance types for a machine, best first (fallbacks for when one has no spot capacity): the smallest size with
/// enough CPU in the family with enough memory per CPU (c: 2 GB, m: 4 GB, r: 8 GB), current generation first.
pub fn aws_instance_types(s: &Spec) -> Vec<String> {
    if let Some(gpu) = s.gpu.as_deref() { return aws_gpu_types(gpu, s) }
    const SIZES: [(u32, &str); 9] = [(2, "large"), (4, "xlarge"), (8, "2xlarge"), (16, "4xlarge"), (32, "8xlarge"), (48, "12xlarge"), (64, "16xlarge"), (96, "24xlarge"), (192, "48xlarge")];
    let arm = s.arch() == "arm64";
    let cpu = s.cpu.unwrap_or(4);
    let ram = s.ram().unwrap_or(if s.cpu.is_none() { 8 } else { cpu * 4 });
    let mut out = vec![];
    for (per_cpu, families) in [(2, if arm { ["c8g", "c7g"] } else { ["c8a", "c7a"] }), (4, if arm { ["m8g", "m7g"] } else { ["m8a", "m7a"] }), (8, if arm { ["r8g", "r7g"] } else { ["r8a", "r7a"] })] {
        let Some((c, size)) = SIZES.iter().find(|(c, _)| *c >= cpu && c * per_cpu >= ram) else { continue };
        for f in families { out.push((*c, per_cpu, format!("{f}.{size}"))) }
    }
    // The fewest CPUs first (what is paid for), then the least memory.
    out.sort_by_key(|(c, per_cpu, _)| (*c, *per_cpu));
    out.into_iter().map(|(_, _, t)| t).collect()
}

/// GPU machines (x64, one NVIDIA GPU) for a machine: the smallest size with enough CPU and memory, in each family
/// with the GPU; for any GPU, the least costly family first (T4, then L4, then A10G).
fn aws_gpu_types(gpu: &str, s: &Spec) -> Vec<String> {
    // (GPU, family, memory per CPU); sizes xlarge (4 CPU) to 16xlarge (64 CPU).
    const FAMILIES: [(&str, &str, u32); 4] = [("t4", "g4dn", 4), ("l4", "g6", 4), ("a10g", "g5", 4), ("l40s", "g6e", 8)];
    const SIZES: [(u32, &str); 5] = [(4, "xlarge"), (8, "2xlarge"), (16, "4xlarge"), (32, "8xlarge"), (64, "16xlarge")];
    let cpu = s.cpu.unwrap_or(4);
    let ram = s.ram().unwrap_or(16);
    FAMILIES.iter().filter(|(g, _, _)| gpu == *g || (gpu == "any" && *g != "l40s"))
        .filter_map(|(_, family, per_cpu)| SIZES.iter().find(|(c, _)| *c >= cpu && c * per_cpu >= ram).map(|(_, size)| format!("{family}.{size}"))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_naming_another_kind_of_machine_does_not_take_the_defaults_kind() {
        let parse = |l: &str| Spec::parse(l, "superci").unwrap().unwrap();
        let arm = Spec { arch: Some("arm64".into()), cpu: Some(8), ..Default::default() };
        assert_eq!((parse("superci").or(&arm).arch(), parse("superci-windows").or(&arm).arch(), parse("superci-gpu").or(&arm).arch()), ("arm64", "x64", "x64"));
        assert_eq!(parse("superci-windows").or(&arm).cpu, Some(8), "its size still comes along");
        let windows = Spec { os: Some("windows".into()), ..Default::default() };
        assert_eq!((parse("superci").or(&windows).os(), parse("superci-arm64").or(&windows).os(), parse("superci-t4").or(&windows).os()), ("windows", "linux", "linux"));
    }

    #[test]
    fn labels() {
        assert_eq!(Spec::parse("superci", "superci"), Some(Ok(Spec::default())));
        assert_eq!(Spec::parse("ubuntu-latest", "superci"), None);
        assert_eq!(Spec::parse("supercix", "superci"), None);
        let s = Spec::parse("SuperCI-arm64-8cpu-32gb-100disk", "superci").unwrap().unwrap();
        assert_eq!((s.cpu, s.ram_gb, s.disk_gb, s.arch()), (Some(8), Some(32), Some(100), "arm64"));
        assert_eq!(Spec::parse("superci-8cpu-arm64", "superci"), Spec::parse("superci-arm64-8cpu", "superci"));
        let s = Spec::parse("superci-macos-aws-ondemand", "superci").unwrap().unwrap();
        assert_eq!((s.os(), s.cloud.as_deref(), s.on_demand), ("macos", Some("aws"), true));
        assert!(Spec::parse("superci-8cores", "superci").unwrap().unwrap_err().contains("8cores"));
        assert!(Spec::parse("superci-500cpu", "superci").unwrap().is_err());
        assert_eq!(Spec::parse("superci-8cpu", "superci").unwrap().unwrap().ram(), Some(32));
        let s = Spec::parse("superci-windows-8cpu", "superci").unwrap().unwrap();
        assert_eq!((s.os(), s.cpu, s.gpu.as_deref()), ("windows", Some(8), None));
        assert_eq!(Spec::parse("superci-gpu", "superci").unwrap().unwrap().gpu.as_deref(), Some("any"));
        assert_eq!(Spec::parse("superci-a10-16cpu", "superci").unwrap().unwrap().gpu.as_deref(), Some("a10g"));
        assert_eq!(Spec::parse("superci-H100", "superci").unwrap().unwrap().describe(), "H100 GPU");
    }

    #[test]
    fn defaults_fill_in() {
        let default = Spec { cpu: Some(2), disk_gb: Some(30), arch: Some("arm64".into()), ..Default::default() };
        let s = Spec { cpu: Some(8), ..Default::default() }.or(&default);
        assert_eq!((s.cpu, s.disk_gb, s.arch()), (Some(8), Some(30), "arm64"));
        assert_eq!(Spec::default().describe(), "the default machine");
        assert_eq!(s.describe(), "8 CPU, 30 GB disk, arm64");
        let with_memory = Spec { cpu: Some(2), ram_gb: Some(8), ..Default::default() };
        assert_eq!(Spec { cpu: Some(8), ..Default::default() }.or(&with_memory).ram(), Some(32), "memory follows the CPU asked for");
        assert_eq!(Spec { disk_gb: Some(100), ..Default::default() }.or(&with_memory).ram(), Some(8));
    }

    #[test]
    fn each_place_sizes_the_machine_itself() {
        let cf = Capacity::cloudflare();
        let size = |label: &str| cf.fit(&Spec::parse(label, "superci").unwrap().unwrap());
        assert_eq!(size("superci"), Ok(Size { cpu: 4, ram_gb: 12, disk_gb: 20, gpu: None }), "the standard is the largest");
        assert_eq!(size("superci-4cpu"), Ok(Size { cpu: 4, ram_gb: 12, disk_gb: 20, gpu: None }), "4 GB per CPU, up to the most it has");
        assert_eq!(size("superci-2cpu"), Ok(Size { cpu: 2, ram_gb: 8, disk_gb: 20, gpu: None }));
        assert_eq!(size("superci-1cpu-2gb"), Ok(Size { cpu: 1, ram_gb: 3, disk_gb: 20, gpu: None }), "at least 3 GB per CPU");
        assert!(size("superci-4cpu-16gb").unwrap_err().contains("12 GB"));
        assert!(size("superci-8cpu").unwrap_err().contains("4 CPU"));
        assert!(size("superci-40disk").unwrap_err().contains("20 GB disk"));
        assert_eq!(Capacity::aws().fit(&Spec { cpu: Some(8), ..Default::default() }), Ok(Size { cpu: 8, ram_gb: 32, disk_gb: 60, gpu: None }));
    }

    #[test]
    fn gpus_and_windows_go_where_they_are() {
        let spec = |label: &str| Spec::parse(label, "superci").unwrap().unwrap();
        // Any GPU: each place's least costly; one by name only where it is.
        assert_eq!(Capacity::modal().fit(&spec("superci-gpu")).unwrap().gpu, Some("t4"));
        assert_eq!(Capacity::modal().fit(&spec("superci-h100")).unwrap().gpu, Some("h100"));
        assert_eq!(Capacity::aws().fit(&spec("superci-l4")).unwrap().gpu, Some("l4"));
        assert_eq!(Capacity::aws().refuses(&spec("superci-h100")), Some("GPUs T4, L4, A10G, L40S only".into()));
        assert_eq!(Capacity::cloudflare().refuses(&spec("superci-gpu")), Some("no GPUs".into()));
        assert_eq!(Capacity::aws().refuses(&spec("superci-gpu-arm64")), Some("GPUs on x64 only".into()));
        // Windows: AWS only, x64, no GPUs.
        assert_eq!(Capacity::aws().refuses(&spec("superci-windows")), None);
        assert!(Capacity::cloudflare().refuses(&spec("superci-windows")).unwrap().contains("linux only"));
        assert!(Capacity::modal().refuses(&spec("superci-windows")).is_some());
        assert_eq!(Capacity::aws().refuses(&spec("superci-windows-arm64")), Some("Windows on x64 only".into()));
        assert_eq!(Capacity::aws().refuses(&spec("superci-windows-gpu")), Some("no Windows machines with GPUs".into()));
        // GPU machines on AWS: the smallest that fits, T4 first for any.
        assert_eq!(aws_instance_types(&spec("superci-gpu")), ["g4dn.xlarge", "g6.xlarge", "g5.xlarge"]);
        assert_eq!(aws_instance_types(&spec("superci-l40s-8cpu")), ["g6e.2xlarge"]);
        assert_eq!(aws_instance_types(&spec("superci-t4-4cpu-32gb")), ["g4dn.2xlarge"]);
    }

    #[test]
    fn places_say_what_they_cannot_run() {
        assert_eq!(Capacity::cloudflare().refuses(&Spec::default()), None);
        assert!(Capacity::cloudflare().refuses(&Spec { cpu: Some(8), ..Default::default() }).unwrap().contains("4 CPU"));
        assert!(Capacity::cloudflare().refuses(&Spec { arch: Some("arm64".into()), ..Default::default() }).is_some());
        assert_eq!(Capacity::aws().refuses(&Spec { cpu: Some(64), arch: Some("arm64".into()), on_demand: true, ..Default::default() }), None);
        assert!(Capacity::aws().refuses(&Spec { os: Some("macos".into()), ..Default::default() }).is_some());
    }

    #[test]
    fn instance_types_fit() {
        // The standard machine (unset size): 4 CPU, 8 GB, as before.
        assert_eq!(aws_instance_types(&Spec::default())[0], "c8a.xlarge");
        let t = aws_instance_types(&Spec { cpu: Some(8), ..Default::default() });
        assert_eq!(t[0], "m8a.2xlarge"); // 32 GB: 4 per CPU
        let t = aws_instance_types(&Spec { cpu: Some(2), ram_gb: Some(16), arch: Some("arm64".into()), ..Default::default() });
        assert_eq!(t[0], "r8g.large");
        let t = aws_instance_types(&Spec { cpu: Some(16), ram_gb: Some(16), ..Default::default() });
        assert_eq!(t[0], "c8a.4xlarge");
        assert!(t.contains(&"m7a.4xlarge".to_string()));
    }
}
