//! What a job asks for. Workflows say `runs-on: superci` for the default machine (set in the dashboard), or say the
//! machine in the label itself, as `key=value` pairs after its name, in any order: `superci/cpu=8`,
//! `superci/cpu=8/ram=32/arch=arm64`, `superci/os=windows`, `superci/gpu=l4`, `superci/provider=aws/spot=false`,
//! `superci/disk=200/disk-speed=500/scratch=ramdisk`, `superci/family=m8a+r8a`. One way to say each thing, and the
//! same keys as `superci machine update` takes. Nothing has to be created first; the machine registers with GitHub
//! under the exact label the job used, so only jobs asking for the same can take it.
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
    /// Which Ubuntu a Linux machine runs, as GitHub's runners: "ubuntu26" (26.04, Docker 29) or "ubuntu24" (24.04,
    /// Docker 28). Unset: as `ubuntu-latest` is (see `ubuntu`). Or an image of the control plane's own, by the name it
    /// was given there (see `valid_image_name`, `plane::RunnerImage`): a container image a job's sandbox starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub image: Option<String>,
    /// One GPU: by name (one of `GPUS`), or "any" (each place's least costly).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub gpu: Option<String>,
    /// Only this provider ("aws", "cloudflare", "modal"): `provider=aws`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cloud: Option<String>,
    /// Whether it may run on a spot machine (one its cloud can take back): unset or `true`, it may; `false`, never
    /// (`spot=false`: a deploy, a job that must not be run twice). A label's `spot=true` undoes a default of `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub spot: Option<bool>,
    /// Docker's images from the cache (Ubuntu 26.04 machines): unset, as its job's last run, or its repository's
    /// jobs', used them (see `layers::plan`); `false`, Docker as it comes, nothing kept after it (`docker-cache=false`:
    /// to time a job cold, or a job the cache does not suit); `true`, from its start whatever its last run used.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub docker_cache: Option<bool>,
    /// Its disk's speed on AWS, when not left to the machine's size (see `disk_speed`): MiB a second, and IOPS.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub disk_mibs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub disk_iops: Option<u32>,
    /// Where what a job writes most (Docker's files, its work folder, /tmp) is kept, when not on the machine's disk:
    /// "ramdisk" (in memory) or "localdisk" (a machine with an NVMe disk of its own, with them on it). One or the
    /// other: `scratch=ramdisk`. AWS, Linux. (A machine that has a local disk uses it without being asked.)
    #[serde(default, skip_serializing_if = "Option::is_none")] pub scratch: Option<String>,
    /// Only these AWS machine families, when a label names any (`family=m8a`, `family=m8a+r8a`): for a job that
    /// must have a certain kind of machine (the newest, say) and would sooner pay or wait than take another.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub families: Vec<String>,
}

/// What a label can say: its keys, in the order they are written when a label is made from a machine.
pub const KEYS: [&str; 14] = ["cpu", "ram", "disk", "disk-speed", "disk-iops", "arch", "os", "image", "gpu", "family", "scratch", "provider", "spot", "docker-cache"];

/// The Ubuntu images a Linux machine can run.
pub const UBUNTUS: [&str; 2] = ["ubuntu26", "ubuntu24"];

/// Whether a word can name an image of a control plane's own (`image=NAME` in a label): two to thirty-two lowercase
/// letters, digits and dashes, a letter first, no dash last. Not a registry's address: a label is lowercased and split at
/// each `/`, and an address has slashes and may have capitals, so a label names the image and the control plane keeps
/// its address. A name beginning with `ubuntu` is SuperCI's (`UBUNTUS`), for the Ubuntus to come.
pub fn valid_image_name(name: &str) -> Result<(), String> {
    let ok = (2..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0].is_ascii_lowercase() && !name.ends_with('-');
    if !ok { return Err("An image's name is 2 to 32 lowercase letters, digits and dashes, starting with a letter (rust-ci, node22).".into()) }
    if name.starts_with("ubuntu") { return Err(format!("{name} begins with ubuntu, which SuperCI keeps for the Ubuntus its machines run: choose another.")) }
    Ok(())
}

/// When a machine that says no image runs Ubuntu 26.04: as GitHub's `ubuntu-latest` does once its move is done
/// (2026-11-19, 00:00 UTC; announced in actions/runner-images#14748). Before, 24.04.
pub const UBUNTU_26_FROM_MS: u64 = 1_795_046_400_000;

/// How a label is written, for a job whose label is not understood.
const HOW: &str = "A label says a machine as key=value pairs after its name, like superci/cpu=8/ram=32. The keys: cpu, ram, disk, disk-speed, disk-iops, arch, os, image, gpu, family, scratch, provider, spot, docker-cache";

/// The AWS machine families a label may name: what a job's CPUs come from (compute, general, memory), AMD and arm64,
/// and those with a local NVMe disk. Each with its memory for each CPU (GB).
pub const FAMILIES: [(&str, u32); 28] = [("c8a", 2), ("c7a", 2), ("m8a", 4), ("m7a", 4), ("r8a", 8), ("r7a", 8), ("c8i", 2), ("c7i", 2), ("m8i", 4), ("m7i", 4), ("c8g", 2), ("c7g", 2), ("m8g", 4), ("m7g", 4), ("r8g", 8), ("r7g", 8),
    ("c8id", 2), ("c6id", 2), ("m8id", 4), ("m6id", 4), ("r8id", 8), ("r6id", 8), ("c8gd", 2), ("c7gd", 2), ("m8gd", 4), ("m7gd", 4), ("r8gd", 8), ("r7gd", 8)];

/// What AWS includes with a disk at no extra price: MiB a second, and IOPS.
pub const DISK_MIBS_INCLUDED: u32 = 125;
pub const DISK_IOPS_INCLUDED: u32 = 3000;

/// How fast a machine's disk is on AWS (MiB a second, IOPS). Left to the machine's size: 31.25 MiB a second for each
/// CPU, no less than what AWS includes and no more than 1,000. So the standard 4-CPU machine has what AWS includes,
/// at no extra price, and the extra on a larger one is a twentieth to a tenth of a spot machine's price (a machine of
/// any size could use more, for its first half hour: its size sets what is worth paying for, not what it can do). IOPS: what AWS includes, or what
/// the speed needs. A job whose files are in memory or on a local disk barely writes to this one: it keeps what AWS
/// includes, unless a speed is said. As asked, when a label or the default machine says: within what a gp3 volume takes (125 to 2,000
/// MiB a second, 3,000 to 80,000 IOPS, 4 IOPS for each MiB a second, 500 IOPS for each GB).
pub fn disk_speed(cpu: u32, disk_gb: u32, s: &Spec) -> (u32, u32) {
    let by_size = if s.scratch.is_some() { DISK_MIBS_INCLUDED } else { (cpu * 125 / 4).clamp(DISK_MIBS_INCLUDED, 1000) };
    let most_iops = (disk_gb.saturating_mul(500)).clamp(DISK_IOPS_INCLUDED, 80_000);
    let mibs = s.disk_mibs.unwrap_or(by_size).clamp(DISK_MIBS_INCLUDED, 2000).min(most_iops / 4);
    let iops = s.disk_iops.unwrap_or(DISK_IOPS_INCLUDED).clamp(DISK_IOPS_INCLUDED, 80_000).max(mibs * 4).min(most_iops);
    (mibs, iops)
}

/// The clouds a label may name.
pub const CLOUDS: [&str; 3] = ["aws", "cloudflare", "modal"];

/// The GPUs a label may name, least costly first.
pub const GPUS: [&str; 8] = ["t4", "l4", "a10g", "l40s", "a100", "h100", "h200", "b200"];

/// Whether a word can be a control plane's label (what workflows name in `runs-on`, before any `/key=value`): two to
/// twenty-four lowercase letters and digits, a letter first, and not a word GitHub's own runners use (`ubuntu`,
/// `linux`…).
pub fn valid_label(label: &str) -> Result<(), String> {
    let ok = (2..=24).contains(&label.len()) && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) && label.as_bytes()[0].is_ascii_lowercase();
    if !ok { return Err("A label is 2 to 24 lowercase letters and digits, starting with a letter (acmeci, ci2).".into()) }
    const TAKEN: [&str; 11] = ["ubuntu", "windows", "macos", "linux", "self", "hosted", "selfhosted", "x64", "arm64", "arm", "latest"];
    if TAKEN.contains(&label) { return Err(format!("{label} is a word GitHub's own runners use: choose another.")) }
    Ok(())
}

/// Whether a control plane can be called this where people read it (AcmeCI, Build Farm): two to twenty-four letters,
/// digits, spaces and dashes, a letter first and no space last. Nothing else, since it goes into an App's name and
/// into what a job's page says, where other marks mean something.
pub fn valid_name(name: &str) -> Result<(), String> {
    let ok = (2..=24).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'-')
        && name.as_bytes()[0].is_ascii_alphabetic() && !name.ends_with([' ', '-']) && !name.contains("  ");
    if ok { Ok(()) } else { Err("A name is 2 to 24 letters, digits, spaces and dashes, starting with a letter (AcmeCI, Build Farm).".into()) }
}

impl Spec {
    /// What a label asks for among several bases a control plane answers to: the longest that fits (so `ci` does not
    /// take `cibig/cpu=8`).
    pub fn parse_among(label: &str, bases: &[String]) -> Option<Result<Spec, String>> {
        let mut bases: Vec<&String> = bases.iter().collect();
        bases.sort_by_key(|b| std::cmp::Reverse(b.len()));
        bases.into_iter().find_map(|b| Spec::parse(label, b))
    }

    /// What a label asks for: `base` alone is the default machine; `base/key=value/…` says one. `None`: not ours;
    /// `Some(Err)`: ours, but not understood (the job is told why instead of waiting forever).
    pub fn parse(label: &str, base: &str) -> Option<Result<Spec, String>> {
        let label = label.to_ascii_lowercase();
        let base = base.to_ascii_lowercase();
        if label == base { return Some(Ok(Spec::default())) }
        let pairs = label.strip_prefix(&base)?.strip_prefix('/')?;
        let mut s = Spec::default();
        let mut said: Vec<&str> = vec![];
        // (A label has at least one pair after its slash, if an empty one.)
        let set = pairs.split('/').try_for_each(|pair| {
            let Some((key, value)) = pair.split_once('=') else { return Err(format!("“{pair}” in runs-on: {label} is not a key=value. {HOW}")) };
            if said.contains(&key) { return Err(format!("{key} is said twice in runs-on: {label}")) }
            said.push(key);
            s.set(key, value).map_err(|e| format!("{e} (runs-on: {label})"))
        });
        Some(set.and_then(|_| s.check().map_err(|e| format!("{e} (runs-on: {label})"))).map(|_| s))
    }

    /// One key of a machine set to a value, as a label or `superci machine update` says it.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let number = |what: &str, least: u32, most: u32| value.parse::<u32>().ok().filter(|n| (least..=most).contains(n)).ok_or_else(|| format!("{key} is {what} from {least} to {most}, not “{value}”"));
        let one_of = |words: &[&str]| if words.contains(&value) { Ok(value.to_string()) } else { Err(format!("{key} is {}, not “{value}”", words.join(" or "))) };
        match key {
            "cpu" => self.cpu = Some(number("a number of CPUs", 1, 192)?),
            "ram" => self.ram_gb = Some(number("GB of memory", 1, 1536)?),
            "disk" => self.disk_gb = Some(number("GB of disk", 1, 16_000)?),
            "disk-speed" => self.disk_mibs = Some(number("MiB a second", DISK_MIBS_INCLUDED, 2000)?),
            "disk-iops" => self.disk_iops = Some(number("IOPS", DISK_IOPS_INCLUDED, 80_000)?),
            "arch" => self.arch = Some(one_of(&["x64", "arm64"])?),
            "os" => self.os = Some(one_of(&["linux", "windows", "macos"])?),
            "image" => self.image = Some(if UBUNTUS.contains(&value) || valid_image_name(value).is_ok() { value.to_string() }
                else { return Err(format!("image is ubuntu26 or ubuntu24, or the name of an image of your own (`superci runner_images`), not “{value}”")) }),
            "gpu" => self.gpu = Some(if value == "any" { value.to_string() } else { one_of(&GPUS).map_err(|_| format!("gpu is any, or one of {}, not “{value}”", GPUS.join(", ")))? }),
            "family" => {
                let named: Vec<String> = value.split('+').map(str::to_string).collect();
                if let Some(unknown) = named.iter().find(|f| !FAMILIES.iter().any(|(known, _)| known == f)) { return Err(format!("family is one or more AWS machine families joined by + (m8a, or m8a+r8a), and “{unknown}” is not one SuperCI knows")) }
                if named.iter().enumerate().any(|(i, f)| named[..i].contains(f)) { return Err(format!("a family is said twice in family={value}")) }
                self.families = named;
            }
            "scratch" => self.scratch = Some(one_of(&["ramdisk", "localdisk"])?),
            "provider" => self.cloud = Some(one_of(&CLOUDS)?),
            "spot" => self.spot = Some(one_of(&["true", "false"])? == "true"),
            "docker-cache" => self.docker_cache = Some(one_of(&["true", "false"])? == "true"),
            _ => return Err(format!("“{key}” is not a key a label has. {HOW}")),
        }
        Ok(())
    }

    /// What does not go together, whatever the provider: said before any machine is asked for.
    pub fn check(&self) -> Result<(), String> {
        let (arm, local) = (|f: &String| f.contains('g'), |f: &String| f.ends_with('d'));
        if let (Some(a), Some(b)) = (self.families.iter().find(|f| arm(f)), self.families.iter().find(|f| !arm(f))) { return Err(format!("{a} machines are arm64 and {b} machines are x64: a job runs on one or the other")) }
        if let (Some(f), Some(arch)) = (self.families.first(), self.arch.as_deref()) { if arm(f) != (arch == "arm64") { return Err(format!("{f} machines are {}, and the label says arch={arch}", if arm(f) { "arm64" } else { "x64" })) } }
        if let (Some(i), true) = (&self.image, self.os() != "linux") { return Err(format!("image={i} is {} for Linux machines, and the label says os={}", if self.own_image().is_some() { "an image" } else { "an Ubuntu" }, self.os())) }
        if let (true, Some(i)) = (self.docker_cache.is_some(), self.image.as_deref().filter(|i| *i != "ubuntu26").filter(|_| self.os() == "linux")) { return Err(format!("the Docker image cache is on Ubuntu 26.04 machines (image=ubuntu26), and the label says image={i}")) }
        if self.docker_cache.is_some() && self.os() != "linux" { return Err(format!("the Docker image cache is on Ubuntu 26.04 machines (image=ubuntu26), and the label says os={}", self.os())) }
        if !self.families.is_empty() && self.gpu.is_some() { return Err("a GPU's machine family follows the GPU: say gpu or family, not both".into()) }
        if self.scratch.as_deref() == Some("localdisk") { if let Some(f) = self.families.iter().find(|f| !local(f)) { return Err(format!("{f} machines have no local disk: name a family that has one (m8id, c8id, r8id; m8gd on arm64), or leave family out")) } }
        Ok(())
    }

    /// The label that says this machine: `base`, then each key that is set, in `KEYS`' order.
    pub fn label(&self, base: &str) -> String {
        let mut out = base.to_string();
        for (key, value) in self.pairs() { out += &format!("/{key}={value}") }
        out
    }

    /// Each key that is set, with its value as a label writes it.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let n = |v: Option<u32>| v.map(|n| n.to_string());
        [("cpu", n(self.cpu)), ("ram", n(self.ram_gb)), ("disk", n(self.disk_gb)), ("disk-speed", n(self.disk_mibs)), ("disk-iops", n(self.disk_iops)), ("arch", self.arch.clone()), ("os", self.os.clone()),
            ("image", self.image.clone()), ("gpu", self.gpu.clone()), ("family", Some(self.families.join("+")).filter(|f| !f.is_empty())), ("scratch", self.scratch.clone()), ("provider", self.cloud.clone()), ("spot", self.spot.map(|s| s.to_string())), ("docker-cache", self.docker_cache.map(|s| s.to_string()))]
            .into_iter().filter_map(|(k, v)| Some((k, v?))).collect()
    }

    /// This, with unset parts taken from `default`.
    pub fn or(&self, default: &Spec) -> Spec {
        Spec {
            // CPU asked for without memory: memory follows the CPU (4 GB each), not the default's.
            cpu: self.cpu.or(default.cpu), ram_gb: self.ram_gb.or(if self.cpu.is_some() { None } else { default.ram_gb }), disk_gb: self.disk_gb.or(default.disk_gb),
            // A label that names another kind of machine (Windows, a GPU, arm64) does not take the default's
            // architecture or system with it (a default of arm64 does not make `os=windows` Windows on arm64).
            arch: self.arch.clone().or_else(|| default.arch.clone().filter(|_| self.os.is_none() && self.gpu.is_none())),
            os: self.os.clone().or_else(|| default.os.clone().filter(|_| self.arch.is_none() && self.gpu.is_none())),
            // (Not the default's Ubuntu for a label that names another system.)
            image: self.image.clone().or_else(|| default.image.clone().filter(|_| self.os.as_deref().is_none_or(|o| o == "linux"))),
            gpu: self.gpu.clone().or_else(|| default.gpu.clone()),
            cloud: self.cloud.clone().or_else(|| default.cloud.clone()), spot: self.spot.or(default.spot), docker_cache: self.docker_cache.or(default.docker_cache),
            disk_mibs: self.disk_mibs.or(default.disk_mibs), disk_iops: self.disk_iops.or(default.disk_iops),
            scratch: self.scratch.clone().or_else(|| default.scratch.clone()),
            families: if self.families.is_empty() { default.families.clone() } else { self.families.clone() },
        }
    }

    /// The architecture: as asked, else its machine family's (`m8g` is arm64), else arm64 for macOS (Apple silicon,
    /// as GitHub's macos-latest) and x64 otherwise.
    pub fn arch(&self) -> &str { self.arch.as_deref().unwrap_or(if self.families.first().is_some_and(|f| f.contains('g')) || self.os() == "macos" { "arm64" } else { "x64" }) }
    /// Never on a spot machine (`spot=false`).
    pub fn on_demand(&self) -> bool { self.spot == Some(false) }
    /// What it writes most in memory, or on a machine's own disk (`scratch`).
    pub fn ramdisk(&self) -> bool { self.scratch.as_deref() == Some("ramdisk") }
    pub fn localdisk(&self) -> bool { self.scratch.as_deref() == Some("localdisk") }
    pub fn os(&self) -> &str { self.os.as_deref().unwrap_or("linux") }
    /// The Ubuntu a Linux machine runs: as asked, else as `ubuntu-latest` is at `now_ms` (see `UBUNTU_26_FROM_MS`).
    pub fn ubuntu(&self, now_ms: u64) -> &str { self.image.as_deref().filter(|i| UBUNTUS.contains(i)).unwrap_or(if now_ms >= UBUNTU_26_FROM_MS { "ubuntu26" } else { "ubuntu24" }) }
    /// The name of an image of the control plane's own it asks for (`image=NAME`), when it names no Ubuntu.
    pub fn own_image(&self) -> Option<&str> { self.image.as_deref().filter(|i| !UBUNTUS.contains(i)) }
    /// Memory: as asked, else 4 GB per CPU.
    pub fn ram(&self) -> Option<u32> { self.ram_gb.or(self.cpu.map(|c| c * 4)) }

    /// In a few words, for the dashboard and errors ("8 CPU, 32 GB, arm64").
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        if let Some(c) = self.cpu { parts.push(format!("{c} CPU")) }
        if let Some(r) = self.ram_gb { parts.push(format!("{r} GB")) }
        if let Some(d) = self.disk_gb { parts.push(format!("{d} GB disk")) }
        if self.arch.is_some() { parts.push(self.arch().to_string()) }
        if self.os.is_some() { parts.push(os_name(self.os()).to_string()) }
        match self.image.as_deref() { Some("ubuntu26") => parts.push("Ubuntu 26.04".into()), Some("ubuntu24") => parts.push("Ubuntu 24.04".into()), Some(i) => parts.push(format!("image {i}")), None => {} }
        match self.gpu.as_deref() { Some("any") => parts.push("a GPU".into()), Some(g) => parts.push(format!("{} GPU", g.to_uppercase())), None => {} }
        if self.on_demand() { parts.push("no spot machines".into()) }
        match self.docker_cache { Some(false) => parts.push("no Docker image cache".into()), Some(true) => parts.push("Docker image cache from the start".into()), None => {} }
        match self.disk_mibs { Some(DISK_MIBS_INCLUDED) => parts.push("standard disk speed".into()), Some(m) => parts.push(format!("{m} MiB/s disk")), None => {} }
        if let Some(i) = self.disk_iops { parts.push(format!("{i} IOPS")) }
        if self.ramdisk() { parts.push("RAM disk".into()) }
        if self.localdisk() { parts.push("local disk".into()) }
        if !self.families.is_empty() { parts.push(format!("only {}", self.families.join(", "))) }
        if let Some(c) = &self.cloud { parts.push(format!("only {c}")) }
        if parts.is_empty() { "the default machine".into() } else { parts.join(", ") }
    }
}

/// What a place can run: the largest machine, which systems, its standard machine (for what a label leaves out), and
/// its memory: per CPU when only CPUs are asked for, and the least it gives per CPU.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Capacity {
    pub max_cpu: u32, pub max_ram_gb: u32, pub max_disk_gb: u32, pub arch: Vec<String>, pub os: Vec<String>, pub on_demand: bool,
    /// The Ubuntu images it can start (a label's `image`).
    pub images: Vec<String>,
    /// Whether it can start a job from an image of the control plane's own, from a container registry (Modal's
    /// sandboxes, see `plane::RunnerImage`).
    #[serde(default)] pub own_images: bool,
    pub std_cpu: u32, pub std_ram_gb: u32, pub std_disk_gb: u32,
    /// Memory for each CPU asked for when the label names no memory (up to the most it has); 0: none promised.
    pub ram_per_cpu: u32,
    /// The least memory it gives per CPU (memory asked for below it is rounded up).
    pub min_ram_per_cpu: u32,
    /// The GPUs it has (one per machine), least costly first; none: no GPUs.
    #[serde(default)] pub gpus: Vec<String>,
    /// Whether it can put a job's files in memory or on a machine's own NVMe disk, and has machine families to
    /// name, when a label asks (AWS).
    #[serde(default)] pub scratch: bool,
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
        if !self.os.iter().any(|o| o == s.os()) { return Err(format!("{} only", self.os.iter().map(|o| os_name(o)).collect::<Vec<_>>().join(", "))) }
        if let (Some(i), false) = (s.own_image(), self.own_images) { return Err(format!("no images of your own (image={i}: Modal only)")) }
        if let Some(i) = s.image.as_deref().filter(|i| !self.images.iter().any(|x| x == i) && s.own_image().is_none()) { return Err(format!("no {i} image (only {})", self.images.join(", "))) }
        if !s.families.is_empty() && !self.scratch { return Err("no AWS machine families".into()) }
        if s.scratch.is_some() && !self.scratch { return Err(format!("no {}", if s.ramdisk() { "RAM disk" } else { "machines with a local disk to ask for" })) }
        if s.scratch.is_some() && s.os() != "linux" { return Err("a RAM disk or a local disk on Linux only".into()) }
        if s.localdisk() && s.gpu.is_some() { return Err("no GPU machines with a local disk to ask for".into()) }
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
    pub fn aws() -> Self { Capacity { max_cpu: 192, max_ram_gb: 1536, max_disk_gb: 16_000, arch: vec!["x64".into(), "arm64".into()], os: vec!["linux".into(), "windows".into()], on_demand: true, images: vec!["ubuntu26".into(), "ubuntu24".into()], own_images: false,
        std_cpu: 4, std_ram_gb: 8, std_disk_gb: 60, ram_per_cpu: 4, min_ram_per_cpu: 0, gpus: ["t4", "l4", "a10g", "l40s"].map(String::from).to_vec(), scratch: true } }
    /// Cloudflare containers, sized per job: 1 to 4 CPU, at least 3 GB per CPU, up to 12 GB and 20 GB disk, x64. CPU is
    /// billed only while used, so the standard machine is the largest: 4 CPU, 12 GB, 20 GB.
    pub fn cloudflare() -> Self { Capacity { max_cpu: 4, max_ram_gb: 12, max_disk_gb: 20, arch: vec!["x64".into()], os: vec!["linux".into()], on_demand: true, images: vec!["ubuntu24".into()], own_images: false,
        std_cpu: 4, std_ram_gb: 12, std_disk_gb: 20, ram_per_cpu: 4, min_ram_per_cpu: 3, gpus: vec![], scratch: false } }
    /// Modal sandboxes (non-preemptible, x64); standard 2 CPU, 8 GB; any of Modal's GPUs, one per sandbox; GitHub's
    /// runner image, or one of the control plane's own.
    pub fn modal() -> Self { Capacity { max_cpu: 64, max_ram_gb: 336, max_disk_gb: 512, arch: vec!["x64".into()], os: vec!["linux".into()], on_demand: true, images: vec!["ubuntu24".into()], own_images: true,
        std_cpu: 2, std_ram_gb: 8, std_disk_gb: 64, ram_per_cpu: 4, min_ram_per_cpu: 0, gpus: GPUS.map(String::from).to_vec(), scratch: false } }
}

/// EC2 instance types for a machine, best first (fallbacks for when one has no spot capacity): the smallest size with
/// enough CPU in the family with enough memory per CPU (c: 2 GB, m: 4 GB, r: 8 GB), current generation first.
/// The sizes an EC2 family has, by CPUs, as AWS lists them (DescribeInstanceTypes, 15 October 2026): all have 2 to
/// 64 CPUs; above, they differ (c8a has no 128, c7a has, c7g ends at 64, c6id at 128). A request that names a type
/// that does not exist is refused whole.
pub fn aws_family_sizes(family: &str) -> Vec<(u32, &'static str)> {
    const UP_TO_64: [(u32, &str); 7] = [(2, "large"), (4, "xlarge"), (8, "2xlarge"), (16, "4xlarge"), (32, "8xlarge"), (48, "12xlarge"), (64, "16xlarge")];
    let more: &[(u32, &'static str)] = match family {
        "c7g" | "m7g" | "r7g" | "c7gd" | "m7gd" | "r7gd" => &[],
        "c7a" | "m7a" | "r7a" | "c8i" | "m8i" | "c8id" | "m8id" | "r8id" => &[(96, "24xlarge"), (128, "32xlarge"), (192, "48xlarge")],
        "c6id" | "m6id" | "r6id" => &[(96, "24xlarge"), (128, "32xlarge")],
        _ => &[(96, "24xlarge"), (192, "48xlarge")],
    };
    UP_TO_64.iter().chain(more).copied().collect()
}

/// The CPUs of an EC2 type, by its size (`c8a.48xlarge`: 192; `xlarge` is 4, `Nxlarge` 4N). None for one not named
/// so (a bare-metal type, a container's size).
pub fn aws_type_cpus(machine_type: &str) -> Option<u32> {
    let size = machine_type.split_once('.')?.1;
    match size { "medium" => Some(1), "large" => Some(2), "xlarge" => Some(4), s => s.strip_suffix("xlarge")?.parse::<u32>().ok().map(|n| n * 4) }
}

pub fn aws_instance_types(s: &Spec) -> Vec<String> {
    if let Some(gpu) = s.gpu.as_deref() { return aws_gpu_types(gpu, s) }
    let arm = s.arch() == "arm64";
    let cpu = s.cpu.unwrap_or(4);
    // (No memory only when no CPUs are asked for either: the standard machine's.)
    let ram = s.ram().unwrap_or(8);
    // (With a local NVMe disk, when asked for: the families that have one.)
    // Only the families a label names, when it names any: each at the smallest size with the CPUs asked for, and
    // the memory asked for or what the family gives (a compute family has half a general one's).
    if !s.families.is_empty() {
        return s.families.iter().filter_map(|f| {
            let per_cpu = FAMILIES.iter().find(|(name, _)| name == f)?.1;
            let (_, size) = aws_family_sizes(f).into_iter().find(|(c, _)| *c >= cpu && s.ram_gb.is_none_or(|r| c * per_cpu >= r))?;
            Some(format!("{f}.{size}"))
        }).collect()
    }
    // (x64: AMD's and Intel's, for more spot pools to choose from; Intel's are often the cheapest.)
    let families: [(u32, &[&str]); 3] = match (s.localdisk(), arm) {
        (false, false) => [(2, &["c8a", "c7a", "c8i", "c7i"]), (4, &["m8a", "m7a", "m8i", "m7i"]), (8, &["r8a", "r7a"])],
        (false, true) => [(2, &["c8g", "c7g"]), (4, &["m8g", "m7g"]), (8, &["r8g", "r7g"])],
        (true, false) => [(2, &["c8id", "c6id"]), (4, &["m8id", "m6id"]), (8, &["r8id", "r6id"])],
        (true, true) => [(2, &["c8gd", "c7gd"]), (4, &["m8gd", "m7gd"]), (8, &["r8gd", "r7gd"])],
    };
    // (Each family at its own smallest size that fits: one may have a size another lacks.)
    let out: Vec<(u32, u32, String)> = families.iter().flat_map(|(per_cpu, families)| families.iter().filter_map(move |f| {
        aws_family_sizes(f).into_iter().find(|(c, _)| *c >= cpu && c * per_cpu >= ram).map(|(c, size)| (c, *per_cpu, format!("{f}.{size}")))
    })).collect();
    // A memory family only where no other gives the memory asked for with as few CPUs: its spot price is often twice
    // a general one's, and AWS, asked for pools by room, took r8a at three times c8a's price.
    let fits_without = |c: u32| out.iter().any(|(oc, per_cpu, _)| *oc == c && *per_cpu < 8);
    let keep: Vec<bool> = out.iter().map(|(c, per_cpu, _)| *per_cpu < 8 || !fits_without(*c)).collect();
    let mut out: Vec<(u32, u32, String)> = out.into_iter().zip(keep).filter(|(_, k)| *k).map(|(o, _)| o).collect();
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
        assert_eq!((parse("superci").or(&arm).arch(), parse("superci/os=windows").or(&arm).arch(), parse("superci/gpu=any").or(&arm).arch()), ("arm64", "x64", "x64"));
        assert_eq!(parse("superci/os=windows").or(&arm).cpu, Some(8), "its size still comes along");
        let windows = Spec { os: Some("windows".into()), ..Default::default() };
        assert_eq!((parse("superci").or(&windows).os(), parse("superci/arch=arm64").or(&windows).os(), parse("superci/gpu=t4").or(&windows).os()), ("windows", "linux", "linux"));
    }

    #[test]
    fn the_ubuntu_follows_ubuntu_latest_unless_asked() {
        let parse = |l: &str| Spec::parse(l, "superci").unwrap();
        let (before, after) = (UBUNTU_26_FROM_MS - 1, UBUNTU_26_FROM_MS);
        assert_eq!((parse("superci").unwrap().ubuntu(before), parse("superci").unwrap().ubuntu(after)), ("ubuntu24", "ubuntu26"));
        assert_eq!((parse("superci/image=ubuntu24").unwrap().ubuntu(after), parse("superci/image=ubuntu26").unwrap().ubuntu(before)), ("ubuntu24", "ubuntu26"));
        assert_eq!(parse("superci/image=ubuntu26/cpu=8").unwrap().label("superci"), "superci/cpu=8/image=ubuntu26");
        assert_eq!(parse("superci/image=ubuntu22").unwrap_err(), "image is ubuntu26 or ubuntu24, or the name of an image of your own (`superci runner_images`), not “ubuntu22” (runs-on: superci/image=ubuntu22)");
        assert!(parse("superci/os=windows/image=ubuntu26").unwrap_err().contains("an Ubuntu for Linux machines"));
        // The Docker image cache: off, or on from the start, for a job; Ubuntu 26.04 only.
        let s = parse("superci/docker-cache=false/image=ubuntu26").unwrap();
        assert_eq!((s.docker_cache, s.label("superci"), s.describe().contains("no Docker image cache")), (Some(false), "superci/image=ubuntu26/docker-cache=false".to_string(), true));
        assert_eq!(parse("superci/docker-cache=true").unwrap().or(&Spec { docker_cache: Some(false), ..Default::default() }).docker_cache, Some(true));
        assert_eq!(parse("superci/cpu=8").unwrap().or(&Spec { docker_cache: Some(false), ..Default::default() }).docker_cache, Some(false));
        assert!(parse("superci/docker-cache=off").unwrap_err().contains("docker-cache is true or false"));
        assert!(parse("superci/docker-cache=true/image=ubuntu24").unwrap_err().contains("the Docker image cache is on Ubuntu 26.04 machines"));
        assert!(parse("superci/docker-cache=false/os=windows").unwrap_err().contains("os=windows"));
        // The default machine's Ubuntu comes along, unless the label names another system.
        let default = Spec { image: Some("ubuntu24".into()), ..Default::default() };
        assert_eq!((parse("superci/cpu=4").unwrap().or(&default).image.as_deref(), parse("superci/os=windows").unwrap().or(&default).image.as_deref()), (Some("ubuntu24"), None));
        assert_eq!(parse("superci/image=ubuntu26").unwrap().or(&default).image.as_deref(), Some("ubuntu26"));
        // Only AWS has 26.04 yet; Cloudflare's and Modal's containers say so.
        let s = parse("superci/image=ubuntu26").unwrap();
        assert!(Capacity::aws().fit(&s).is_ok());
        assert_eq!(Capacity::cloudflare().fit(&s).unwrap_err(), "no ubuntu26 image (only ubuntu24)");
        assert!(Capacity::modal().fit(&parse("superci/image=ubuntu24").unwrap()).is_ok());
    }

    #[test]
    fn an_image_of_ones_own_is_named_in_a_label_and_only_modal_starts_from_one() {
        let parse = |l: &str| Spec::parse(l, "superci").unwrap();
        let s = parse("superci/image=rust-ci/cpu=8").unwrap();
        assert_eq!((s.own_image(), s.ubuntu(UBUNTU_26_FROM_MS), s.label("superci"), s.describe()), (Some("rust-ci"), "ubuntu26", "superci/cpu=8/image=rust-ci".to_string(), "8 CPU, image rust-ci".to_string()));
        assert_eq!((parse("superci/image=ubuntu24").unwrap().own_image(), parse("superci").unwrap().own_image()), (None, None), "an Ubuntu is no image of one's own");
        // A label is lowercased and split at each slash: a registry's address is not a value it can hold.
        assert_eq!(parse("superci/image=ghcr.io/acme/ci:1").unwrap_err(), "image is ubuntu26 or ubuntu24, or the name of an image of your own (`superci runner_images`), not “ghcr.io” (runs-on: superci/image=ghcr.io/acme/ci:1)");
        for bad in ["ghcr.io", "ci:1", "-ci", "ci-", "x", "rust_ci", "ubuntu-rust", &"a".repeat(33)] {
            assert!(parse(&format!("superci/image={bad}")).unwrap_err().contains("or the name of an image of your own"), "{bad}");
        }
        assert!(valid_image_name("rust-ci").is_ok() && valid_image_name("node22").is_ok() && valid_image_name("ubuntu-ci").unwrap_err().contains("begins with ubuntu"));
        // A Linux image, with no Docker image cache (that is Ubuntu 26.04's on AWS).
        assert_eq!(parse("superci/image=rust-ci/os=windows").unwrap_err(), "image=rust-ci is an image for Linux machines, and the label says os=windows (runs-on: superci/image=rust-ci/os=windows)");
        assert!(parse("superci/image=rust-ci/docker-cache=false").unwrap_err().contains("and the label says image=rust-ci"));
        // Modal's sandboxes start from one; AWS's machines and Cloudflare's containers say they cannot.
        assert_eq!(Capacity::modal().fit(&s), Ok(Size { cpu: 8, ram_gb: 32, disk_gb: 64, gpu: None }));
        assert_eq!(Capacity::aws().refuses(&s), Some("no images of your own (image=rust-ci: Modal only)".into()));
        assert_eq!(Capacity::cloudflare().refuses(&parse("superci/image=rust-ci").unwrap()), Some("no images of your own (image=rust-ci: Modal only)".into()));
        assert!(Capacity::modal().fit(&parse("superci/image=ubuntu26").unwrap()).is_err(), "nor does Modal have 26.04 for it");
        // The default machine's image comes along, as an Ubuntu does.
        assert_eq!(parse("superci/cpu=4").unwrap().or(&Spec { image: Some("rust-ci".into()), ..Default::default() }).own_image(), Some("rust-ci"));
    }

    #[test]
    fn labels() {
        assert_eq!(Spec::parse("superci", "superci"), Some(Ok(Spec::default())));
        assert_eq!(Spec::parse("ubuntu-latest", "superci"), None);
        assert_eq!(Spec::parse("supercix", "superci"), None);
        let s = Spec::parse("SuperCI/cpu=8/ram=32/disk=100/arch=arm64", "superci").unwrap().unwrap();
        assert_eq!((s.cpu, s.ram_gb, s.disk_gb, s.arch()), (Some(8), Some(32), Some(100), "arm64"));
        assert_eq!(Spec::parse("superci/cpu=8/arch=arm64", "superci"), Spec::parse("superci/cpu=8/arch=arm64", "superci"));
        let s = Spec::parse("superci/os=macos/provider=aws/spot=false", "superci").unwrap().unwrap();
        assert_eq!((s.os(), s.cloud.as_deref(), s.on_demand(), s.spot), ("macos", Some("aws"), true, Some(false)));
        // What is not understood says so, with how a label is written: a key that is none, a value out of range or
        // not one of its words, a pair with no "=", a key said twice.
        let err = |label: &str| Spec::parse(label, "superci").unwrap().unwrap_err();
        assert!(err("superci/cores=8").starts_with("“cores” is not a key a label has. A label says a machine as key=value pairs after its name, like superci/cpu=8/ram=32. The keys: cpu, ram, disk, disk-speed, disk-iops, arch, os, image, gpu, family, scratch, provider, spot, docker-cache"), "{}", err("superci/cores=8"));
        assert_eq!(err("superci/cpu=500"), "cpu is a number of CPUs from 1 to 192, not “500” (runs-on: superci/cpu=500)");
        assert_eq!(err("superci/arch=arm"), "arch is x64 or arm64, not “arm” (runs-on: superci/arch=arm)");
        assert!(err("superci/cpu=8/ondemand").starts_with("“ondemand” in runs-on: superci/cpu=8/ondemand is not a key=value."));
        assert_eq!(err("superci/cpu=8/cpu=16"), "cpu is said twice in runs-on: superci/cpu=8/cpu=16");
        assert!(err("superci/spot=no").contains("spot is true or false") && err("superci/cpu=").contains("not “”"));
        // Only the name alone, or the name and a slash, is this control plane's label.
        assert_eq!((Spec::parse("superci-8cpu", "superci"), Spec::parse("superci8", "superci"), Spec::parse("superci/", "superci").map(|r| r.is_err())), (None, None, Some(true)));
        // A machine written back as its label: each key that is set, in one order.
        assert_eq!(Spec::parse("superci/spot=false/arch=arm64/cpu=8", "superci").unwrap().unwrap().label("superci"), "superci/cpu=8/arch=arm64/spot=false");
        assert_eq!((Spec::default().label("acmeci"), Spec::parse("superci/spot=true", "superci").unwrap().unwrap().spot), ("acmeci".to_string(), Some(true)));
        assert_eq!(Spec::parse("superci/cpu=8", "superci").unwrap().unwrap().ram(), Some(32));
        let s = Spec::parse("superci/cpu=8/os=windows", "superci").unwrap().unwrap();
        assert_eq!((s.os(), s.cpu, s.gpu.as_deref()), ("windows", Some(8), None));
        assert_eq!(Spec::parse("superci/gpu=any", "superci").unwrap().unwrap().gpu.as_deref(), Some("any"));
        assert_eq!(Spec::parse("superci/cpu=16/gpu=a10g", "superci").unwrap().unwrap().gpu.as_deref(), Some("a10g"));
        assert_eq!(Spec::parse("superci/gpu=h100", "superci").unwrap().unwrap().describe(), "H100 GPU");
        // A disk's speed, and where a job's files go.
        let s = Spec::parse("superci/cpu=8/disk-speed=500/disk-iops=6000/scratch=ramdisk", "superci").unwrap().unwrap();
        assert_eq!((s.disk_mibs, s.disk_iops, s.ramdisk(), s.localdisk(), s.disk_gb), (Some(500), Some(6000), true, false, None));
        assert_eq!(s.describe(), "8 CPU, 500 MiB/s disk, 6000 IOPS, RAM disk");
        assert!(Spec::parse("superci/disk-speed=50", "superci").unwrap().unwrap_err().contains("from 125 to 2000") && Spec::parse("superci/disk-iops=100", "superci").unwrap().is_err());
        let nvme = Spec::parse("superci/cpu=16/scratch=localdisk", "superci").unwrap().unwrap();
        assert!(err("superci/scratch=nvme").contains("scratch is ramdisk or localdisk") && err("superci/scratch=ramdisk/scratch=localdisk").contains("said twice"), "one or the other");
        assert_eq!(nvme.describe(), "16 CPU, local disk");
        assert_eq!(aws_instance_types(&nvme)[..2], ["m8id.4xlarge".to_string(), "m6id.4xlarge".to_string()]);
        assert_eq!(aws_instance_types(&Spec { arch: Some("arm64".into()), ..nvme.clone() })[0], "m8gd.4xlarge");
        assert!(Capacity::cloudflare().refuses(&nvme).is_some() && Capacity::modal().refuses(&s).is_some() && Capacity::aws().refuses(&s).is_none());
        assert!(Capacity::aws().refuses(&Spec::parse("superci/os=windows/scratch=ramdisk", "superci").unwrap().unwrap()).unwrap().contains("Linux only"));
        // A machine family, when a job must have it: only those named, in the label's order, at the size that fits.
        let pinned = Spec::parse("superci/cpu=32/family=m8a+r8a", "superci").unwrap().unwrap();
        assert_eq!((aws_instance_types(&pinned), pinned.describe()), (vec!["m8a.8xlarge".to_string(), "r8a.8xlarge".to_string()], "32 CPU, only m8a, r8a".to_string()));
        assert_eq!(aws_instance_types(&Spec::parse("superci/cpu=16/family=c8a", "superci").unwrap().unwrap()), ["c8a.4xlarge"], "the family's own memory, when none is asked for");
        assert_eq!(aws_instance_types(&Spec::parse("superci/cpu=16/ram=64/family=c8a", "superci").unwrap().unwrap()), ["c8a.8xlarge"], "larger, for the memory asked");
        assert!(Capacity::aws().refuses(&pinned).is_none() && Capacity::modal().refuses(&pinned).is_some());
        // A family says its architecture; what does not go together is said at once.
        let arm = Spec::parse("superci/family=m8g", "superci").unwrap().unwrap();
        assert_eq!((arm.arch(), Capacity::aws().refuses(&arm), aws_instance_types(&arm)), ("arm64", None, vec!["m8g.xlarge".to_string()]));
        assert_eq!(err("superci/arch=x64/family=m8g"), "m8g machines are arm64, and the label says arch=x64 (runs-on: superci/arch=x64/family=m8g)");
        assert!(err("superci/family=m8a+m8g").contains("a job runs on one or the other") && err("superci/family=m9z").contains("“m9z” is not one SuperCI knows") && err("superci/family=m8a+m8a").contains("said twice"));
        assert!(err("superci/gpu=l4/family=m8a").contains("say gpu or family, not both"));
        assert!(err("superci/scratch=localdisk/family=m8a").starts_with("m8a machines have no local disk: name a family that has one"));
        assert_eq!(aws_instance_types(&Spec::parse("superci/cpu=8/scratch=localdisk/family=m8id", "superci").unwrap().unwrap()), ["m8id.2xlarge"]);
        assert!(valid_label("m8a").is_ok() && valid_label("gpu").is_ok() && valid_label("linux").is_err(), "only GitHub's own words are taken");
        // The disk's speed follows the machine's size unless said; what is said stays within what a gp3 volume takes.
        let d = Spec::default();
        assert_eq!([2, 4, 8, 16, 32, 96].map(|cpu| disk_speed(cpu, 60, &d)), [(125, 3000), (125, 3000), (250, 3000), (500, 3000), (1000, 4000), (1000, 4000)]);
        assert_eq!(disk_speed(4, 60, &Spec { disk_mibs: Some(125), ..Default::default() }), (125, 3000), "the standard speed, when the default machine says so");
        assert_eq!(disk_speed(4, 60, &Spec { disk_mibs: Some(2000), ..Default::default() }), (2000, 8000));
        assert_eq!(disk_speed(8, 60, &Spec { disk_iops: Some(80_000), ..Default::default() }), (250, 30_000), "500 IOPS for each GB at most");
        assert_eq!(disk_speed(32, 8, &d), (1000, 4000));
        // With its files in memory or on a local disk, the disk is not what a job writes to: what AWS includes, unless said.
        let scratch = |on: &str| Spec { scratch: Some(on.into()), ..Default::default() };
        assert_eq!((disk_speed(32, 60, &scratch("ramdisk")), disk_speed(32, 60, &scratch("localdisk"))), ((125, 3000), (125, 3000)));
        assert_eq!(disk_speed(32, 60, &Spec { disk_mibs: Some(500), ..scratch("ramdisk") }), (500, 3000));
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
        assert_eq!(size("superci/cpu=4"), Ok(Size { cpu: 4, ram_gb: 12, disk_gb: 20, gpu: None }), "4 GB per CPU, up to the most it has");
        assert_eq!(size("superci/cpu=2"), Ok(Size { cpu: 2, ram_gb: 8, disk_gb: 20, gpu: None }));
        assert_eq!(size("superci/cpu=1/ram=2"), Ok(Size { cpu: 1, ram_gb: 3, disk_gb: 20, gpu: None }), "at least 3 GB per CPU");
        assert!(size("superci/cpu=4/ram=16").unwrap_err().contains("12 GB"));
        assert!(size("superci/cpu=8").unwrap_err().contains("4 CPU"));
        assert!(size("superci/disk=40").unwrap_err().contains("20 GB disk"));
        assert_eq!(Capacity::aws().fit(&Spec { cpu: Some(8), ..Default::default() }), Ok(Size { cpu: 8, ram_gb: 32, disk_gb: 60, gpu: None }));
    }

    #[test]
    fn gpus_and_windows_go_where_they_are() {
        let spec = |label: &str| Spec::parse(label, "superci").unwrap().unwrap();
        // Any GPU: each place's least costly; one by name only where it is.
        assert_eq!(Capacity::modal().fit(&spec("superci/gpu=any")).unwrap().gpu, Some("t4"));
        assert_eq!(Capacity::modal().fit(&spec("superci/gpu=h100")).unwrap().gpu, Some("h100"));
        assert_eq!(Capacity::aws().fit(&spec("superci/gpu=l4")).unwrap().gpu, Some("l4"));
        assert_eq!(Capacity::aws().refuses(&spec("superci/gpu=h100")), Some("GPUs T4, L4, A10G, L40S only".into()));
        assert_eq!(Capacity::cloudflare().refuses(&spec("superci/gpu=any")), Some("no GPUs".into()));
        assert_eq!(Capacity::aws().refuses(&spec("superci/arch=arm64/gpu=any")), Some("GPUs on x64 only".into()));
        // Windows: AWS only, x64, no GPUs.
        assert_eq!(Capacity::aws().refuses(&spec("superci/os=windows")), None);
        assert!(Capacity::cloudflare().refuses(&spec("superci/os=windows")).unwrap().contains("Linux only"));
        assert!(Capacity::modal().refuses(&spec("superci/os=windows")).is_some());
        assert_eq!(Capacity::aws().refuses(&spec("superci/arch=arm64/os=windows")), Some("Windows on x64 only".into()));
        assert_eq!(Capacity::aws().refuses(&spec("superci/os=windows/gpu=any")), Some("no Windows machines with GPUs".into()));
        // GPU machines on AWS: the smallest that fits, T4 first for any.
        assert_eq!(aws_instance_types(&spec("superci/gpu=any")), ["g4dn.xlarge", "g6.xlarge", "g5.xlarge"]);
        assert_eq!(aws_instance_types(&spec("superci/cpu=8/gpu=l40s")), ["g6e.2xlarge"]);
        assert_eq!(aws_instance_types(&spec("superci/cpu=4/ram=32/gpu=t4")), ["g4dn.2xlarge"]);
    }

    #[test]
    fn places_say_what_they_cannot_run() {
        assert_eq!(Capacity::cloudflare().refuses(&Spec::default()), None);
        assert!(Capacity::cloudflare().refuses(&Spec { cpu: Some(8), ..Default::default() }).unwrap().contains("4 CPU"));
        assert!(Capacity::cloudflare().refuses(&Spec { arch: Some("arm64".into()), ..Default::default() }).is_some());
        assert_eq!(Capacity::aws().refuses(&Spec { cpu: Some(64), arch: Some("arm64".into()), spot: Some(false), ..Default::default() }), None);
        assert!(Capacity::aws().refuses(&Spec { os: Some("macos".into()), ..Default::default() }).is_some());
    }

    #[test]
    fn instance_types_fit() {
        // The standard machine (unset size): 4 CPU, 8 GB, as before.
        assert_eq!(aws_instance_types(&Spec::default())[0], "c8a.xlarge");
        // Above 64 CPUs, each family's own sizes: 128 is c7a's (c8a has none), then 192; arm64 has no 7th-generation
        // size above 64, and none asks for a type that does not exist.
        let big = |l: &str| aws_instance_types(&Spec::parse(l, "superci").unwrap().unwrap());
        // (r7a, with more memory than asked, only where no general family has the size: here m7a and m8i do.)
        assert_eq!(big("superci/cpu=128")[..3], ["m7a.32xlarge", "m8i.32xlarge", "m8a.48xlarge"].map(String::from));
        assert!(!big("superci/cpu=128").iter().any(|t| t == "r7a.32xlarge"), "{:?}", big("superci/cpu=128"));
        // 64 CPUs and their memory: the general families, AMD's and Intel's, then compute ones large enough for the
        // memory; no memory family beside them.
        assert_eq!(big("superci/cpu=64"), ["m8a.16xlarge", "m7a.16xlarge", "m8i.16xlarge", "m7i.16xlarge", "c7a.32xlarge", "c8i.32xlarge", "c8a.48xlarge", "c7i.48xlarge"].map(String::from));
        assert_eq!(big("superci/cpu=128/family=c8a"), ["c8a.48xlarge"], "a family without the size: its next");
        assert_eq!(big("superci/cpu=96/arch=arm64"), ["m8g.24xlarge", "c8g.48xlarge"].map(String::from));
        assert!(big("superci/cpu=192/scratch=localdisk").iter().all(|t| !t.starts_with("c6id") && !t.starts_with("m6id") && !t.starts_with("r6id")));
        assert_eq!((aws_type_cpus("c8a.48xlarge"), aws_type_cpus("c7a.32xlarge"), aws_type_cpus("m8g.large"), aws_type_cpus("c8a.metal-48xl"), aws_type_cpus("4cpu-12gb")), (Some(192), Some(128), Some(2), None, None));
        let t = aws_instance_types(&Spec { cpu: Some(8), ..Default::default() });
        assert_eq!(t[0], "m8a.2xlarge"); // 32 GB: 4 per CPU
        let t = aws_instance_types(&Spec { cpu: Some(2), ram_gb: Some(16), arch: Some("arm64".into()), ..Default::default() });
        assert_eq!(t[0], "r8g.large");
        let t = aws_instance_types(&Spec { cpu: Some(16), ram_gb: Some(16), ..Default::default() });
        assert_eq!(t[0], "c8a.4xlarge");
        assert!(t.contains(&"m7a.4xlarge".to_string()));
    }

    #[test]
    fn every_family_has_its_sizes_from_2_cpus_to_its_largest() {
        let largest = |f: &str| match f { "c7g" | "m7g" | "r7g" | "c7gd" | "m7gd" | "r7gd" => 64, "c6id" | "m6id" | "r6id" => 128, _ => 192 };
        for (f, _) in FAMILIES {
            let sizes = aws_family_sizes(f);
            assert_eq!((sizes.first().copied(), sizes.last().map(|s| s.0)), (Some((2, "large")), Some(largest(f))), "{f}");
            assert!(sizes.iter().all(|(c, size)| aws_type_cpus(&format!("{f}.{size}")) == Some(*c)), "{f}");
            assert_eq!(sizes.iter().any(|(c, _)| *c == 128), ["c7a", "m7a", "r7a", "c8i", "m8i", "c8id", "m8id", "r8id", "c6id", "m6id", "r6id"].contains(&f), "{f}");
        }
        assert_eq!((aws_type_cpus("t3.medium"), aws_type_cpus("c8a.xlarge"), aws_type_cpus("c8a.bigxlarge")), (Some(1), Some(4), None));
    }

    #[test]
    fn every_key_says_what_it_takes_when_given_what_it_does_not() {
        let err = |label: &str| Spec::parse(label, "superci").unwrap().unwrap_err();
        for (label, said) in [
            ("superci/ram=0", "ram is GB of memory from 1 to 1536, not “0”"), ("superci/disk=16001", "disk is GB of disk from 1 to 16000, not “16001”"),
            ("superci/os=bsd", "os is linux or windows or macos, not “bsd”"), ("superci/provider=gcp", "provider is aws or cloudflare or modal, not “gcp”"),
            ("superci/gpu=v100", "gpu is any, or one of t4, l4, a10g, l40s, a100, h100, h200, b200, not “v100”"),
        ] {
            assert_eq!(err(label), format!("{said} (runs-on: {label})"));
        }
        for os in OS_NAMES.map(|(id, _)| id) { assert!(Spec::parse(&format!("superci/os={os}"), "superci").unwrap().is_ok(), "{os}") }
        assert_eq!(OS_NAMES.map(|(id, _)| os_name(id)), ["Linux", "Windows", "macOS"]);
        assert_eq!(os_name("plan9"), "plan9");
        // A family and the architecture it is: together when they agree.
        assert!(Spec::parse("superci/arch=arm64/family=m8g", "superci").unwrap().is_ok() && Spec::parse("superci/arch=x64/family=m8a", "superci").unwrap().is_ok());
    }

    #[test]
    fn every_part_of_a_machine_is_described() {
        let s = Spec::parse("superci/ram=16/image=ubuntu24/gpu=any/docker-cache=true/disk-speed=125/provider=modal", "superci").unwrap();
        assert_eq!(s.unwrap_err(), "the Docker image cache is on Ubuntu 26.04 machines (image=ubuntu26), and the label says image=ubuntu24 (runs-on: superci/ram=16/image=ubuntu24/gpu=any/docker-cache=true/disk-speed=125/provider=modal)");
        let s = Spec { ram_gb: Some(16), os: Some("linux".into()), image: Some("ubuntu24".into()), gpu: Some("any".into()), docker_cache: Some(true), disk_mibs: Some(DISK_MIBS_INCLUDED), cloud: Some("modal".into()), ..Default::default() };
        assert_eq!(s.describe(), "16 GB, Linux, Ubuntu 24.04, a GPU, Docker image cache from the start, standard disk speed, only modal");
        assert_eq!(Spec { image: Some("ubuntu26".into()), ..Default::default() }.describe(), "Ubuntu 26.04");
    }

    #[test]
    fn a_local_disk_or_a_gpu_is_refused_where_there_is_none() {
        let s = |label: &str| Spec::parse(label, "superci").unwrap().unwrap();
        assert_eq!(Capacity::cloudflare().refuses(&s("superci/scratch=localdisk")), Some("no machines with a local disk to ask for".into()));
        assert_eq!(Capacity::cloudflare().refuses(&s("superci/scratch=ramdisk")), Some("no RAM disk".into()));
        assert_eq!(Capacity::aws().refuses(&s("superci/scratch=localdisk/gpu=t4")), Some("no GPU machines with a local disk to ask for".into()));
        // A family SuperCI does not know, or one without a size with the CPUs asked for, gives no machine type.
        assert!(aws_instance_types(&Spec { families: vec!["z9".into()], ..Default::default() }).is_empty());
        assert!(aws_instance_types(&Spec { cpu: Some(500), families: vec!["m8a".into()], ..Default::default() }).is_empty());
    }
}

/// The operating systems a label may name, each as people write it.
pub const OS_NAMES: [(&str, &str); 3] = [("linux", "Linux"), ("windows", "Windows"), ("macos", "macOS")];

/// An operating system as people write it: Linux, Windows, macOS (another: as it is).
pub fn os_name(os: &str) -> &str { OS_NAMES.iter().find(|(id, _)| *id == os).map_or(os, |(_, name)| *name) }
