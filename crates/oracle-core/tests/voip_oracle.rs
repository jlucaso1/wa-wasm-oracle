//! The VoIP module as a behavioural oracle.
//!
//! These tests call the real WhatsApp Web calling engine and assert on what it
//! returns. They exist to be compared against whatsapp-rust: any constant or
//! behaviour asserted here is the ground truth the Rust implementation has to
//! match, taken from the shipped artifact rather than from a decompilation.
//!
//! They skip when the capture directory is absent. A skipped run is not a
//! passing run — check for `skipping:` in the output.

use oracle_core::{Catalog, Runtime, Value};

const VOIP_MODULE: &str = "D5pLH9sfOOl";

/// Loads the VoIP module with its constructors already run, or `None` when the
/// capture is not available.
fn voip() -> Option<Runtime> {
    let catalog = Catalog::discover().ok()?;
    let module = catalog.resolve(VOIP_MODULE).ok()?;
    let bytes = std::fs::read(&module.path).ok()?;

    let mut runtime = Runtime::instantiate(&bytes).expect("VoIP module should instantiate");
    runtime
        .run_ctors()
        .expect("VoIP constructors should run to completion");
    runtime.clear_calls();
    Some(runtime)
}

macro_rules! voip_or_skip {
    () => {
        match voip() {
            Some(runtime) => runtime,
            None => {
                eprintln!("skipping: no VoIP capture (set WA_WASM_DIR)");
                return;
            }
        }
    };
}

#[test]
fn registers_its_signaling_api() {
    let mut runtime = voip_or_skip!();
    let registry = runtime.embind();

    // The entry points that carry protocol behaviour. If a capture stops
    // exporting one of these, the oracle can no longer answer for it, and that
    // should fail loudly rather than silently narrow the test surface.
    for expected in [
        "handleIncomingSignalingOffer",
        "handleIncomingSignalingMessage",
        "handleIncomingSignalingAck",
        "handleIncomingSignalingReceipt",
        "startVoipCall",
        "acceptCall",
        "rejectCall",
        "endCall",
    ] {
        assert!(
            registry
                .functions
                .iter()
                .any(|function| function.name == expected),
            "VoIP module should register `{expected}`"
        );
    }

    assert!(
        registry.functions.len() > 90,
        "expected the full API, got {} functions",
        registry.functions.len()
    );
}

#[test]
fn reports_its_p2p_virtual_endpoint() {
    let mut runtime = voip_or_skip!();

    // Documentation addresses (RFC 5737 / RFC 3849). The engine uses them as
    // placeholders for the P2P path rather than real routable addresses.
    assert_eq!(
        runtime.call_embind("getWebP2PVirtualIpv4", &[]).unwrap(),
        Value::Str("192.0.2.1".to_owned())
    );
    assert_eq!(
        runtime.call_embind("getWebP2PVirtualIpv6", &[]).unwrap(),
        Value::Str("2001:db8::1".to_owned())
    );
    assert_eq!(
        runtime.call_embind("getWebP2PVirtualPort", &[]).unwrap(),
        Value::Int(9999)
    );
}

#[test]
fn reports_its_sctp_ice_event_ids() {
    let mut runtime = voip_or_skip!();

    // Wire constants: these ids go into telemetry events, so a Rust
    // implementation that logs them must use the same numbers.
    for (function, expected) in [
        ("getTsLoggerEventIdWebSctpIceConnectionStart", 199),
        ("getTsLoggerEventIdWebSctpIceConnectionComplete", 200),
        ("getTsLoggerEventIdWebSctpIceConnectionFailed", 201),
    ] {
        assert_eq!(
            runtime.call_embind(function, &[]).unwrap(),
            Value::Int(expected),
            "{function}"
        );
    }
}

/// The property the whole harness rests on: identical input, identical output.
/// Without it, a difference between this and whatsapp-rust could never be
/// attributed to the implementation rather than to the run.
#[test]
fn is_deterministic_across_instances() {
    let Some(mut first) = voip() else {
        eprintln!("skipping: no VoIP capture (set WA_WASM_DIR)");
        return;
    };
    let mut second = voip().expect("second instance");

    let probes: [(&str, Vec<Value>); 5] = [
        ("getWebP2PVirtualIpv4", vec![]),
        ("getWebP2PVirtualIpv6", vec![]),
        ("getWebP2PVirtualPort", vec![]),
        ("getCallInfo", vec![]),
        ("getEncodedVideoPortMask", vec![]),
    ];

    for (function, args) in &probes {
        let a = first.call_embind(function, args);
        let b = second.call_embind(function, args);
        assert_eq!(
            a.as_ref().ok(),
            b.as_ref().ok(),
            "`{function}` differed between two runs"
        );
    }
}

/// Calling the same function repeatedly in one instance must also be stable —
/// a test that compares against whatsapp-rust will do exactly this.
#[test]
fn is_deterministic_across_repeated_calls() {
    let mut runtime = voip_or_skip!();

    let first = runtime.call_embind("getWebP2PVirtualIpv4", &[]).unwrap();
    for _ in 0..8 {
        assert_eq!(
            runtime.call_embind("getWebP2PVirtualIpv4", &[]).unwrap(),
            first
        );
    }
}

#[test]
fn accepts_abprop_overrides() {
    let mut runtime = voip_or_skip!();

    // Property setters are the documented way to steer engine behaviour, and
    // they must not throw for an unknown key.
    assert_eq!(
        runtime
            .call_embind(
                "setABPropBool",
                &[Value::Str("test_prop".to_owned()), Value::Bool(true)]
            )
            .unwrap(),
        Value::Void
    );
    assert_eq!(
        runtime
            .call_embind(
                "setABPropInt",
                &[Value::Str("test_int".to_owned()), Value::Int(42)]
            )
            .unwrap(),
        Value::Void
    );
}
