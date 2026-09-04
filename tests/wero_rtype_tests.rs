//! WERO reservation-type contracts — the `fix/wero-rtype` rung.
//!
//! Rung-2 bring-up (`.benchmarks`-grade live measurement, 2026-08-15,
//! kernel nvmet on 7.1.6-sqz) proved `src/meta_backend/reservation.rs`'s
//! `RTYPE_WRITE_EXCLUSIVE_REGISTRANTS_ONLY = 2` is **not WERO**: per the
//! NVMe spec (and the kernel's `enum nvme_pr_type`,
//! `include/linux/nvme.h`), rtype 2 is **Exclusive Access** — under such
//! a hold a REGISTERED second host is refused reads AND writes. Every
//! consumer that MEANS Write Exclusive – Registrants Only (the S7
//! `data_custody` WeroHold, the S9 co-writer admission's `is_wero()`
//! rung, the VL2b job-wire coordinator fence) was therefore taking a
//! stricter hold than designed: registrant co-writers would be denied
//! all I/O, and readers would be denied during remote jobs.
//!
//! Pinned here, red-first:
//!
//! * **The wire values, against the kernel enum** (`nvme_pr_type`:
//!   WRITE_EXCLUSIVE = 1, EXCLUSIVE_ACCESS = 2,
//!   WRITE_EXCLUSIVE_REG_ONLY = 3): the product's registrants-only
//!   acquire must put rtype **3** on the wire, `is_wero()` must read
//!   rtype 3 as WERO and must NEVER read rtype 2 (Exclusive Access) as
//!   WERO — the constant can never silently regress.
//! * **The SPEC semantics at the fake seam**: under the product's WERO
//!   hold a registered non-holder writes; an unregistered host is
//!   refused; D0's rtype-1 Write Exclusive still admits only the holder.
//! * **The SPEC semantics LIVE** (capability class — root + kernel
//!   nvmet + nvme-cli; the environment skip routes through
//!   squeezefs-testkit and is promoted by
//!   `SQUEEZEFS_TEST_REQUIRE_CAPABILITY=1`, so the zc-capability gate
//!   runs this leg on the sqz box): the PRODUCT reservation client
//!   (`NvmeReservationClient`) takes the hold on a real nvmet-tcp
//!   namespace (resv_enable=1, built by the product `nvmeof` verbs);
//!   the device's own Reservation Report must echo rtype 3; a SECOND
//!   registrant (its own fabrics association, distinct hostnqn/hostid —
//!   the rung-2 per-connection identity) must be admitted READS **and**
//!   WRITES while registered, and refused writes once unregistered.
//!   nvme-cli drives the second association's instrument commands
//!   per-controller through its CHAR device (the
//!   `mw_two_registrants_leg.sh` discipline — the head block node
//!   round-robins paths on native-multipath kernels; the char device
//!   pins the association).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use squeezefs::meta_backend::reservation::{
    FakeNvmeNamespace, FakeReservationClient, NvmeReservationClient, ReservationClient,
    ReservationRegistrant, ReservationReport,
};
use squeezefs_testkit::{site, SkipClass};

/// `include/linux/nvme.h` `enum nvme_pr_type` — the authority the
/// constants are pinned against (NVMe Base Spec, Reservation Type):
///   1 = Write Exclusive, 2 = Exclusive Access,
///   3 = Write Exclusive – Registrants Only,
///   4 = Exclusive Access – Registrants Only,
///   5 = Write Exclusive – All Registrants,
///   6 = Exclusive Access – All Registrants.
const NVME_PR_WRITE_EXCLUSIVE: u8 = 1;
const NVME_PR_EXCLUSIVE_ACCESS: u8 = 2;
const NVME_PR_WRITE_EXCLUSIVE_REG_ONLY: u8 = 3;

fn fake_pair() -> (
    Arc<FakeNvmeNamespace>,
    Arc<FakeReservationClient>,
    Arc<FakeReservationClient>,
) {
    let ns = FakeNvmeNamespace::new();
    let a = FakeReservationClient::new(ns.clone(), "nqn.host-a", "host-a");
    let b = FakeReservationClient::new(ns.clone(), "nqn.host-b", "host-b");
    (ns, a, b)
}

// ===========================================================================
// The wire values — the constant pinned against the kernel enum
// ===========================================================================

#[test]
fn wero_acquire_puts_nvme_rtype_3_on_the_wire() {
    let (_ns, a, _b) = fake_pair();
    a.register(0xA).expect("register A");
    a.acquire_write_exclusive_registrants_only(0xA)
        .expect("WERO acquire");
    let report = a.report().expect("report");
    assert_eq!(
        report.rtype, NVME_PR_WRITE_EXCLUSIVE_REG_ONLY,
        "Write Exclusive – Registrants Only is NVMe rtype 3 (kernel enum \
         nvme_pr_type::NVME_PR_WRITE_EXCLUSIVE_REG_ONLY); rtype 2 is EXCLUSIVE \
         ACCESS, under which a registered second host is refused reads AND \
         writes (measured live on kernel nvmet, 2026-08-15)"
    );
    assert!(
        report.is_wero(),
        "the product's own WERO acquire must read back as WERO"
    );
}

#[test]
fn write_exclusive_acquire_stays_nvme_rtype_1() {
    let (_ns, a, _b) = fake_pair();
    a.register(0xA).expect("register A");
    a.acquire_write_exclusive(0xA).expect("WE acquire");
    let report = a.report().expect("report");
    assert_eq!(
        report.rtype, NVME_PR_WRITE_EXCLUSIVE,
        "D0's Write Exclusive is NVMe rtype 1 — untouched by the WERO fix"
    );
    assert!(
        !report.is_wero(),
        "a Write Exclusive hold is not a registrants-only hold"
    );
}

#[test]
fn an_exclusive_access_hold_never_reads_as_wero() {
    // The regression pin: rtype 2 is EXCLUSIVE ACCESS. A report carrying
    // it must NEVER satisfy the S9 co-writer admission's rung-5 probe
    // (`is_wero`) — admitting a co-writer under an Exclusive Access hold
    // is admitting a mount whose every read and write the device rejects.
    let report = ReservationReport {
        holder_key: Some(0xA),
        registrants: vec![ReservationRegistrant {
            rkey: 0xA,
            host_id: vec![0xAA; 16],
            holds_reservation: true,
        }],
        rtype: NVME_PR_EXCLUSIVE_ACCESS,
    };
    assert!(
        !report.is_wero(),
        "rtype 2 (Exclusive Access) must never read as WERO — the constant \
         regressed to the pre-fix value"
    );
    let wero = ReservationReport {
        rtype: NVME_PR_WRITE_EXCLUSIVE_REG_ONLY,
        ..report
    };
    assert!(wero.is_wero(), "rtype 3 is WERO");
}

// ===========================================================================
// The SPEC semantics at the fake seam
// ===========================================================================

#[test]
fn wero_admits_registered_non_holders_and_refuses_unregistered_hosts() {
    let (ns, a, b) = fake_pair();
    a.register(0xA).expect("register A");
    b.register(0xB).expect("register B");
    a.acquire_write_exclusive_registrants_only(0xA)
        .expect("WERO acquire");

    let wire_b = b.wire_host_id().expect("B wire id");
    assert!(
        ns.write_allowed(&wire_b),
        "a REGISTERED non-holder writes under WERO (registrants only admits)"
    );
    assert!(
        !ns.write_allowed(b"unregistered-third-host"),
        "an unregistered host is refused under WERO"
    );

    // Once B unregisters, the device refuses its writes too.
    b.unregister(0xB).expect("B self-unregister");
    assert!(
        !ns.write_allowed(&wire_b),
        "a host that unregistered is refused under the standing WERO hold"
    );
}

#[test]
fn write_exclusive_still_admits_only_the_holder() {
    let (ns, a, b) = fake_pair();
    a.register(0xA).expect("register A");
    b.register(0xB).expect("register B");
    a.acquire_write_exclusive(0xA).expect("WE acquire");
    assert!(ns.write_allowed(&a.wire_host_id().expect("A wire id")));
    assert!(
        !ns.write_allowed(&b.wire_host_id().expect("B wire id")),
        "D0's rtype-1 semantics untouched: even a registered non-holder is \
         write-blocked under plain Write Exclusive"
    );
}

// ===========================================================================
// The SPEC semantics LIVE — kernel nvmet, product client, capability class
// ===========================================================================

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

fn sqz(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn squeezefs {args:?}: {e}"))
}

fn nvme_cli(args: &[&str]) -> std::process::Output {
    Command::new("nvme")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn nvme {args:?}: {e}"))
}

/// The controller (`nvmeN`) whose sysfs attrs carry `subsysnqn` +
/// `hostnqn`, polled up to ~10 s (fabric attach is asynchronous).
fn wait_controller(subnqn: &str, hostnqn: &str) -> Option<String> {
    for _ in 0..40 {
        if let Ok(rd) = std::fs::read_dir("/sys/class/nvme") {
            let mut names: Vec<_> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            names.sort();
            for dir in names {
                let attr = |a: &str| {
                    std::fs::read_to_string(dir.join(a))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default()
                };
                if attr("subsysnqn") == subnqn && attr("hostnqn") == hostnqn {
                    return dir.file_name().map(|n| n.to_string_lossy().into_owned());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

/// The namespace BLOCK node under `subnqn` (multipath head or plain),
/// polled: `/sys/class/nvme-subsystem/*/nvme*n*` then controller-class.
fn wait_block_node(subnqn: &str) -> Option<String> {
    for _ in 0..40 {
        for root in ["/sys/class/nvme-subsystem", "/sys/class/nvme"] {
            let Ok(rd) = std::fs::read_dir(root) else {
                continue;
            };
            for e in rd.filter_map(|e| e.ok()) {
                let dir = e.path();
                let nqn = std::fs::read_to_string(dir.join("subsysnqn"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if nqn != subnqn {
                    continue;
                }
                let Ok(children) = std::fs::read_dir(&dir) else {
                    continue;
                };
                let mut names: Vec<String> = children
                    .filter_map(|c| c.ok())
                    .filter(|c| c.path().is_dir())
                    .map(|c| c.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        // Strict ^nvme\d+n\d+$ (the initiator's shape rule).
                        n.strip_prefix("nvme")
                            .map(|rest| {
                                let mut it = rest.splitn(2, 'n');
                                matches!(
                                    (it.next(), it.next()),
                                    (Some(a), Some(b))
                                        if !a.is_empty()
                                            && !b.is_empty()
                                            && a.bytes().all(|c| c.is_ascii_digit())
                                            && b.bytes().all(|c| c.is_ascii_digit())
                                )
                            })
                            .unwrap_or(false)
                    })
                    .collect();
                names.sort();
                if let Some(n) = names.into_iter().next() {
                    let path = format!("/dev/{n}");
                    if Path::new(&path).exists() {
                        return Some(path);
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

/// Best-effort teardown guard: the test must never strand fabric
/// residue, panic or not.
struct LiveVenue {
    nqn: String,
    backing: PathBuf,
}

impl Drop for LiveVenue {
    fn drop(&mut self) {
        let _ = sqz(&["nvmeof", "disconnect", &self.nqn]);
        // Controllers take a beat to tear down before unshare.
        for _ in 0..20 {
            let live = std::fs::read_dir("/sys/class/nvme")
                .map(|rd| {
                    rd.filter_map(|e| e.ok()).any(|e| {
                        std::fs::read_to_string(e.path().join("subsysnqn"))
                            .map(|s| s.trim() == self.nqn)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if !live {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let _ = sqz(&["nvmeof", "unshare", &self.nqn]);
        let _ = std::fs::remove_file(&self.backing);
    }
}

/// Instrument write/read through a controller CHAR device (association-
/// pinned on native-multipath kernels). `Ok(())` = admitted.
///
/// Issued as an NVM `io-passthru` (opcode 0x02 READ / 0x01 WRITE, one
/// 512-byte block at LBA 0) rather than `nvme read`/`nvme write`: on
/// nvme-cli 1.x (EL8's 1.16) those two open the char device and probe it
/// with `ioctl(BLKSSZGET)`, which a controller node answers ENOTTY, so
/// they exit nonzero with NO output — the empty-string failure the 1.2 zc
/// gate hit on the sqz box, with every product step of the leg already
/// green. The passthru form is the same command on the wire and every
/// nvme-cli speaks it on a char device.
fn char_io(ctrl: &str, write: bool) -> Result<(), String> {
    let dev = format!("/dev/{ctrl}");
    let mut args: Vec<&str> = vec![
        "io-passthru",
        &dev,
        "--namespace-id=1",
        if write {
            "--opcode=0x01"
        } else {
            "--opcode=0x02"
        },
        "--data-len=512",
        // cdw10/11 = SLBA 0, cdw12 = NLB 0 (one block).
        "--cdw10=0",
        "--cdw11=0",
        "--cdw12=0",
    ];
    if write {
        args.push("--write");
        args.push("--input-file=/dev/zero");
    } else {
        args.push("--read");
    }
    let out = nvme_cli(&args);
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// The live SPEC-semantics pin. RED against the pre-fix constant: the
/// product acquire took an Exclusive Access hold (rtype 2), so the
/// device echoed rtype 2 and refused the registered second host's reads
/// and writes (measured 2026-08-15 — the rung-2 bring-up conviction).
#[test]
fn live_wero_admits_registered_second_association_and_refuses_unregistered() {
    // Capability preconditions (each a ledgered, promotable skip):
    if unsafe { libc::geteuid() } != 0 {
        let _ = squeezefs_testkit::declare(
            site!(),
            SkipClass::Capability,
            "root required (fabrics connect + kernel nvmet configfs)",
        );
        return;
    }
    for m in ["nvmet", "nvmet-tcp", "nvme-tcp"] {
        let _ = Command::new("modprobe").arg(m).status();
    }
    if !Path::new("/sys/kernel/config/nvmet").exists() {
        let _ = squeezefs_testkit::declare(
            site!(),
            SkipClass::Capability,
            "kernel nvmet (configfs) unavailable",
        );
        return;
    }
    if Command::new("nvme").arg("--version").output().is_err() {
        let _ = squeezefs_testkit::declare(
            site!(),
            SkipClass::Capability,
            "nvme-cli unavailable (the second association's instrument)",
        );
        return;
    }

    let pid = std::process::id();
    let nqn = format!("nqn.2026-08.io.squeezefs:werortype-{pid}");
    let backing = PathBuf::from(format!("/tmp/sqz-werortype-{pid}.img"));
    // Port 54146: inside the tcp devsub service slice 54100–54199, never
    // the fidelity tier's 54000–54099.
    let port = "54146";
    let host_a = format!("nqn.2014-08.org.nvmexpress:uuid:aaaaaaaa-2026-0815-1111-{pid:012}");
    let hostid_a = format!("aaaaaaaa-2026-0815-1111-{pid:012}");
    let host_b = format!("nqn.2014-08.org.nvmexpress:uuid:bbbbbbbb-2026-0815-1111-{pid:012}");
    let hostid_b = format!("bbbbbbbb-2026-0815-1111-{pid:012}");

    // Product target: nvmet-tcp on localhost, resv_enable=1 by default.
    let out = sqz(&[
        "nvmeof",
        "share",
        backing.to_str().expect("utf8 backing"),
        "--create-size",
        "64M",
        "--subnqn",
        &nqn,
        "--ip",
        "127.0.0.1",
        "--port",
        port,
        "--target-stack",
        "nvmet",
    ]);
    assert!(
        out.status.success(),
        "product share failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let venue = LiveVenue {
        nqn: nqn.clone(),
        backing,
    };

    // Association A FIRST and ALONE, so the head node has exactly one
    // path while the PRODUCT client registers/acquires — passthru on the
    // head is unambiguously A's association.
    let out = sqz(&[
        "nvmeof",
        "connect",
        "--ip",
        "127.0.0.1",
        "--port",
        port,
        "--subnqn",
        &nqn,
        "--hostnqn",
        &host_a,
        "--hostid",
        &hostid_a,
    ]);
    assert!(
        out.status.success(),
        "product connect (A) failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let ctrl_a = wait_controller(&nqn, &host_a).expect("controller A attaches");
    let node = wait_block_node(&nqn).expect("namespace block node appears");

    let client = NvmeReservationClient::open(Path::new(&node))
        .expect("the product reservation client opens the namespace");
    const KEY_A: u64 = 0x0202_600A;
    client.register(KEY_A).expect("product register (A)");
    client
        .acquire_write_exclusive_registrants_only(KEY_A)
        .expect("product WERO acquire (A)");

    // The device's own echo of the hold: rtype 3 = WERO per the kernel
    // enum. rtype 2 here means the constant regressed to Exclusive
    // Access — the live face of `wero_acquire_puts_nvme_rtype_3_on_the_wire`.
    let report = client.report().expect("product report");
    assert_eq!(
        report.rtype, NVME_PR_WRITE_EXCLUSIVE_REG_ONLY,
        "the real target must echo rtype 3 (WERO) for the product's \
         registrants-only acquire"
    );
    assert!(report.is_wero(), "the product hold reads back as WERO");

    // Association B: its own fabrics connection, its own registrant
    // (the rung-2 per-connection identity).
    let out = sqz(&[
        "nvmeof",
        "connect",
        "--ip",
        "127.0.0.1",
        "--port",
        port,
        "--subnqn",
        &nqn,
        "--hostnqn",
        &host_b,
        "--hostid",
        &hostid_b,
    ]);
    assert!(
        out.status.success(),
        "product connect (B) failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let ctrl_b = wait_controller(&nqn, &host_b).expect("controller B attaches");
    assert_ne!(ctrl_a, ctrl_b, "two associations, two controllers");

    // Controller attach is asynchronous past the sysfs attrs: the char
    // device's nsid-1 passthru answers ENOTTY until the controller's
    // namespace scan attaches the ns (its `nvme*n*` path-node child
    // appears under the controller dir). Wait, then retry the register
    // briefly — a persistent failure is a real failure.
    let ctrl_dir = PathBuf::from("/sys/class/nvme").join(&ctrl_b);
    for _ in 0..40 {
        let has_ns = std::fs::read_dir(&ctrl_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok()).any(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    e.path().is_dir() && n.starts_with("nvme") && n.contains('n')
                })
            })
            .unwrap_or(false);
        if has_ns {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let dev_b = format!("/dev/{ctrl_b}");
    let mut registered = false;
    let mut last_err = String::new();
    for _ in 0..20 {
        let out = nvme_cli(&[
            "resv-register",
            &dev_b,
            "-n",
            "1",
            "--nrkey=0x202600b",
            "--cptpl=0",
        ]);
        if out.status.success() {
            registered = true;
            break;
        }
        last_err = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert!(registered, "register (B) failed after retries: {last_err}");

    // THE SPEC SEMANTICS: a registered non-holder READS and WRITES under
    // the WERO hold. (Under the pre-fix rtype-2 Exclusive Access hold
    // both were refused with the reservation-conflict class.)
    char_io(&ctrl_b, false).expect("registered non-holder READS under WERO");
    char_io(&ctrl_b, true).expect("registered non-holder WRITES under WERO");

    // …and an unregistered host is refused writes: B unregisters itself,
    // then its write must conflict while its read stays admitted (WERO
    // restricts writes only).
    let out = nvme_cli(&[
        "resv-register",
        &dev_b,
        "-n",
        "1",
        "--crkey=0x202600b",
        "--rrega=1",
    ]);
    assert!(
        out.status.success(),
        "B self-unregister failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let refused = char_io(&ctrl_b, true).expect_err("an unregistered host is write-refused");
    // nvme-cli 2.x prints "Reservation Conflict", 1.x "RESERVATION_CONFLICT".
    assert!(
        refused
            .to_ascii_lowercase()
            .replace('_', " ")
            .contains("reservation conflict"),
        "the refusal is the reservation-conflict class: {refused}"
    );
    char_io(&ctrl_b, false)
        .expect("reads stay admitted for unregistered hosts under WERO (write-exclusive class)");

    // The holder still writes; product release + unregister leave zero
    // PR residue (the venue guard tears the fabric down).
    char_io(&ctrl_a, true).expect("the holder writes");
    client
        .release_registrants_only(KEY_A)
        .expect("product WERO release");
    drop(venue);
}
