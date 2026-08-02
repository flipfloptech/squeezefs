//! Fuzz the **IPC bootstrap blob** and the **ctl message** codec
//! (`crates/squeezefs-ipc/src/wire.rs`).
//!
//! The bootstrap blob arrives as an xattr value the shim reads out of a
//! mount it has not yet authenticated (VAL-4's subject), and ctl messages
//! arrive on an abstract AF_UNIX socket. `tests/ipc_host_tests.rs` covers
//! every NAMED refusal class (version, nonce, flags, mode, budget,
//! peercred, disabled); this target covers the ones nobody named
//! (spec §11 TEST-4).
//!
//! Invariants: both decoders are total, and anything that decodes
//! re-encodes to something that decodes identically — a decoder that
//! accepts a form its own encoder cannot produce is an ABI hole.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs_ipc::wire::{BootstrapBlob, CtlMsg, CTL_MSG_MAX};

fuzz_target!(|data: &[u8]| {
    if let Ok(blob) = BootstrapBlob::decode(data) {
        let re = blob.encode();
        assert_eq!(
            BootstrapBlob::decode(&re).ok(),
            Some(blob),
            "the bootstrap blob must round-trip"
        );
    }

    if let Ok(msg) = CtlMsg::decode(data) {
        let re = msg.encode();
        assert!(
            re.len() <= CTL_MSG_MAX,
            "an accepted ctl message re-encoded to {} B, past the {CTL_MSG_MAX} B \
             receive-buffer bound",
            re.len()
        );
        assert_eq!(
            CtlMsg::decode(&re).ok(),
            Some(msg),
            "a ctl message must round-trip"
        );
    }
});
