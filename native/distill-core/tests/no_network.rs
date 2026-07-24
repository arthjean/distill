#![cfg(target_os = "linux")]
#![allow(clippy::expect_used)]

use distill_core::{
    Budget, ByteString, CONTRACT_VERSION, CountUnit, Engine, EngineConfig, Request, Retention,
    Source,
};
use std::{
    collections::BTreeMap,
    io,
    net::{SocketAddr, TcpStream},
};

#[test]
fn capture_projection_restore_and_gc_run_under_a_network_deny_filter() {
    install_network_deny_filter().expect("install seccomp filter");
    let denied = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], 9)))
        .expect_err("network filter must reject sockets");
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);

    let directory = tempfile::tempdir().expect("temp directory");
    let engine = Engine::new(EngineConfig::local(
        directory.path().join("store/store.sqlite"),
    ))
    .expect("engine");
    let projected = engine
        .handle(request(
            "network-probe",
            Source::Inline {
                bytes: ByteString::from_utf8("raw source"),
                media_type: None,
            },
        ))
        .expect("project");
    let restored = engine
        .handle(request(
            "network-restore",
            Source::Artifact {
                artifact: projected.artifact,
            },
        ))
        .expect("restore");
    assert_eq!(restored.visible.bytes, "raw source");
}

fn request(request_id: &str, source: Source) -> Request {
    Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: request_id.to_owned(),
        source,
        budget: Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: 128,
            reserved_envelope: 0,
            token_profile: None,
        },
        preservation_profile: "plain-text/v1".to_owned(),
        retention: Retention::default(),
        metadata: BTreeMap::new(),
    }
}

fn install_network_deny_filter() -> io::Result<()> {
    const BPF_LD: u16 = 0x00;
    const BPF_W: u16 = 0x00;
    const BPF_ABS: u16 = 0x20;
    const BPF_JMP: u16 = 0x05;
    const BPF_JEQ: u16 = 0x10;
    const BPF_K: u16 = 0x00;
    const BPF_RET: u16 = 0x06;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;

    let filters = [
        libc::sock_filter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: 1,
            k: libc::SYS_socket as u32,
        },
        libc::sock_filter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: 1,
            k: libc::SYS_connect as u32,
        },
        libc::sock_filter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        },
    ];
    let program = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_ptr().cast_mut(),
    };
    // SAFETY: PR_SET_NO_NEW_PRIVS and PR_SET_SECCOMP consume integer arguments
    // plus a pointer to the live, correctly sized filter program above.
    let no_new_privileges = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if no_new_privileges != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the filter program remains alive for the duration of this call.
    let installed = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        )
    };
    if installed != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
