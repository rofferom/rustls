// This program does assorted benchmarking of rustls.
//
// Note: we don't use any of the standard 'cargo bench', 'test::Bencher',
// etc. because it's unstable at the time of writing.

use std::fs;
use std::io::{self, Read, Write};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use pki_types::{CertificateDer, PrivateKeyDer};
use rustls::client::Resumption;
#[cfg(all(not(feature = "ring"), feature = "aws_lc_rs"))]
use rustls::crypto::aws_lc_rs as provider;
#[cfg(all(not(feature = "ring"), feature = "aws_lc_rs"))]
use rustls::crypto::aws_lc_rs::{cipher_suite, Ticketer};
#[cfg(feature = "ring")]
use rustls::crypto::ring as provider;
#[cfg(feature = "ring")]
use rustls::crypto::ring::{cipher_suite, Ticketer};
use rustls::crypto::CryptoProvider;
use rustls::server::{NoServerSessionStorage, ServerSessionMemoryCache, WebPkiClientVerifier};
use rustls::{
    ClientConfig, ClientConnection, ConnectionCommon, RootCertStore, ServerConfig,
    ServerConnection, SideData,
};

pub fn main() {
    let args = Args::parse();

    let options = Options {
        work_multiplier: args.multiplier,
    };

    match args.command() {
        Command::Bulk {
            cipher_suite,
            plaintext_size,
            max_fragment_size,
        } => {
            for param in lookup_matching_benches(cipher_suite).iter() {
                bench_bulk(param, &options, *plaintext_size, *max_fragment_size);
            }
        }

        Command::Handshake { cipher_suite }
        | Command::HandshakeResume { cipher_suite }
        | Command::HandshakeTicket { cipher_suite } => {
            let resume = ResumptionParam::from_subcommand(args.command());

            for param in lookup_matching_benches(cipher_suite).iter() {
                bench_handshake(param, &options, ClientAuth::No, resume);
            }
        }
        Command::Memory {
            cipher_suite,
            count,
        } => {
            for param in lookup_matching_benches(cipher_suite).iter() {
                bench_memory(param, *count);
            }
        }
        Command::ListSuites => {
            for bench in ALL_BENCHMARKS {
                println!(
                    "{:?} (key={:?} version={:?})",
                    bench.ciphersuite, bench.key_type, bench.version
                );
            }
        }
        Command::AllTests => {
            all_tests(&options);
        }
    }
}

#[derive(Parser, Debug)]
#[command(version, about = "Runs rustls benchmarks")]
struct Args {
    #[arg(
        long,
        default_value_t = 1.0,
        env = "BENCH_MULTIPLIER",
        help = "Multiplies the length of every test by the given float value"
    )]
    multiplier: f64,

    #[command(subcommand)]
    command: Option<Command>,
}

impl Args {
    fn command(&self) -> &Command {
        self.command
            .as_ref()
            .unwrap_or(&Command::AllTests)
    }
}

#[derive(Parser, Debug)]
enum Command {
    #[command(about = "Runs bulk data benchmarks")]
    Bulk {
        #[arg(help = "Which cipher suite to use; see `list-suites` for possible values.")]
        cipher_suite: String,

        #[arg(default_value_t = 1048576, help = "The size of each data write")]
        plaintext_size: u64,

        #[arg(help = "Maximum TLS fragment size")]
        max_fragment_size: Option<usize>,
    },

    #[command(about = "Runs full handshake speed benchmarks")]
    Handshake {
        #[arg(help = "Which cipher suite to use; see `list-suites` for possible values.")]
        cipher_suite: String,
    },

    #[command(about = "Runs stateful resumed handshake speed benchmarks")]
    HandshakeResume {
        #[arg(help = "Which cipher suite to use; see `list-suites` for possible values.")]
        cipher_suite: String,
    },

    #[command(about = "Runs stateless resumed handshake speed benchmarks")]
    HandshakeTicket {
        #[arg(help = "Which cipher suite to use; see `list-suites` for possible values.")]
        cipher_suite: String,
    },

    #[command(
        about = "Runs memory benchmarks",
        long_about = "This creates `count` connections in parallel (count / 2 clients connected\n\
                      to count / 2 servers), and then moves them in lock-step though the handshake.\n\
                      Once the handshake completes the client writes 1KB of data to the server."
    )]
    Memory {
        #[arg(help = "Which cipher suite to use; see `list-suites` for possible values.")]
        cipher_suite: String,

        #[arg(
            default_value_t = 1000000,
            help = "How many connections to create in parallel"
        )]
        count: u64,
    },

    #[command(about = "Lists the supported values for cipher-suite options")]
    ListSuites,

    #[command(about = "Run all tests (the default subcommand)")]
    AllTests,
}

fn all_tests(options: &Options) {
    for test in ALL_BENCHMARKS.iter() {
        bench_bulk(test, options, 1024 * 1024, None);
        bench_bulk(test, options, 1024 * 1024, Some(10000));
        bench_handshake(test, options, ClientAuth::No, ResumptionParam::No);
        bench_handshake(test, options, ClientAuth::Yes, ResumptionParam::No);
        bench_handshake(test, options, ClientAuth::No, ResumptionParam::SessionId);
        bench_handshake(test, options, ClientAuth::Yes, ResumptionParam::SessionId);
        bench_handshake(test, options, ClientAuth::No, ResumptionParam::Tickets);
        bench_handshake(test, options, ClientAuth::Yes, ResumptionParam::Tickets);
    }
}

fn bench_handshake(
    params: &BenchmarkParam,
    options: &Options,
    clientauth: ClientAuth,
    resume: ResumptionParam,
) {
    let client_config = Arc::new(make_client_config(params, clientauth, resume));
    let server_config = Arc::new(make_server_config(params, clientauth, resume, None));

    assert!(params.ciphersuite.version() == params.version);

    let rounds = options.apply_work_multiplier(if resume == ResumptionParam::No {
        512
    } else {
        4096
    });
    let mut client_time = 0f64;
    let mut server_time = 0f64;

    for _ in 0..rounds {
        let server_name = "localhost".try_into().unwrap();
        let mut client = ClientConnection::new(Arc::clone(&client_config), server_name).unwrap();
        let mut server = ServerConnection::new(Arc::clone(&server_config)).unwrap();

        server_time += time(|| {
            transfer(&mut client, &mut server, None);
        });
        client_time += time(|| {
            transfer(&mut server, &mut client, None);
        });
        server_time += time(|| {
            transfer(&mut client, &mut server, None);
        });
        client_time += time(|| {
            transfer(&mut server, &mut client, None);
        });
    }

    println!(
        "handshakes\t{:?}\t{:?}\t{:?}\tclient\t{}\t{}\t{:.2}\thandshake/s",
        params.version,
        params.key_type,
        params.ciphersuite.suite(),
        if clientauth == ClientAuth::Yes {
            "mutual"
        } else {
            "server-auth"
        },
        resume.label(),
        (rounds as f64) / client_time
    );
    println!(
        "handshakes\t{:?}\t{:?}\t{:?}\tserver\t{}\t{}\t{:.2}\thandshake/s",
        params.version,
        params.key_type,
        params.ciphersuite.suite(),
        if clientauth == ClientAuth::Yes {
            "mutual"
        } else {
            "server-auth"
        },
        resume.label(),
        (rounds as f64) / server_time
    );
}

fn bench_bulk(
    params: &BenchmarkParam,
    options: &Options,
    plaintext_size: u64,
    max_fragment_size: Option<usize>,
) {
    let client_config = Arc::new(make_client_config(
        params,
        ClientAuth::No,
        ResumptionParam::No,
    ));
    let server_config = Arc::new(make_server_config(
        params,
        ClientAuth::No,
        ResumptionParam::No,
        max_fragment_size,
    ));

    let total_data = options.apply_work_multiplier(if plaintext_size < 8192 {
        64 * 1024 * 1024
    } else {
        1024 * 1024 * 1024
    });
    let rounds = total_data / plaintext_size;

    report_bulk_result(
        "bulk",
        bench_bulk_buffered(client_config, server_config, plaintext_size, rounds),
        plaintext_size,
        rounds,
        max_fragment_size,
        params,
    );
}

fn bench_bulk_buffered(
    client_config: Arc<ClientConfig>,
    server_config: Arc<ServerConfig>,
    plaintext_size: u64,
    rounds: u64,
) -> (f64, f64) {
    let server_name = "localhost".try_into().unwrap();
    let mut client = ClientConnection::new(client_config, server_name).unwrap();
    client.set_buffer_limit(None);
    let mut server = ServerConnection::new(server_config).unwrap();
    server.set_buffer_limit(None);

    do_handshake(&mut client, &mut server);

    let mut time_send = 0f64;
    let mut time_recv = 0f64;

    let buf = vec![0; plaintext_size as usize];
    for _ in 0..rounds {
        time_send += time(|| {
            server.writer().write_all(&buf).unwrap();
        });

        time_recv += transfer(&mut server, &mut client, Some(buf.len()));
    }

    (time_send, time_recv)
}

fn report_bulk_result(
    variant: &str,
    (time_send, time_recv): (f64, f64),
    plaintext_size: u64,
    rounds: u64,
    max_fragment_size: Option<usize>,
    params: &BenchmarkParam,
) {
    let mfs_str = format!(
        "max_fragment_size:{}",
        max_fragment_size
            .map(|v| v.to_string())
            .unwrap_or_else(|| "default".to_string())
    );
    let total_mbs = ((plaintext_size * rounds) as f64) / (1024. * 1024.);
    println!(
        "{}\t{:?}\t{:?}\t{}\tsend\t{:.2}\tMB/s",
        variant,
        params.version,
        params.ciphersuite.suite(),
        mfs_str,
        total_mbs / time_send
    );
    println!(
        "{}\t{:?}\t{:?}\t{}\trecv\t{:.2}\tMB/s",
        variant,
        params.version,
        params.ciphersuite.suite(),
        mfs_str,
        total_mbs / time_recv
    );
}

fn bench_memory(params: &BenchmarkParam, conn_count: u64) {
    let client_config = Arc::new(make_client_config(
        params,
        ClientAuth::No,
        ResumptionParam::No,
    ));
    let server_config = Arc::new(make_server_config(
        params,
        ClientAuth::No,
        ResumptionParam::No,
        None,
    ));

    // The target here is to end up with conn_count post-handshake
    // server and client sessions.
    let conn_count = (conn_count / 2) as usize;
    let mut servers = Vec::with_capacity(conn_count);
    let mut clients = Vec::with_capacity(conn_count);

    for _i in 0..conn_count {
        servers.push(ServerConnection::new(Arc::clone(&server_config)).unwrap());
        let server_name = "localhost".try_into().unwrap();
        clients.push(ClientConnection::new(Arc::clone(&client_config), server_name).unwrap());
    }

    for _step in 0..5 {
        for (client, server) in clients
            .iter_mut()
            .zip(servers.iter_mut())
        {
            do_handshake_step(client, server);
        }
    }

    for client in clients.iter_mut() {
        client
            .writer()
            .write_all(&[0u8; 1024])
            .unwrap();
    }

    for (client, server) in clients
        .iter_mut()
        .zip(servers.iter_mut())
    {
        transfer(client, server, Some(1024));
    }
}

fn make_server_config(
    params: &BenchmarkParam,
    client_auth: ClientAuth,
    resume: ResumptionParam,
    max_fragment_size: Option<usize>,
) -> ServerConfig {
    let provider = Arc::new(provider::default_provider());
    let client_auth = match client_auth {
        ClientAuth::Yes => {
            let roots = params.key_type.get_chain();
            let mut client_auth_roots = RootCertStore::empty();
            for root in roots {
                client_auth_roots.add(root).unwrap();
            }
            WebPkiClientVerifier::builder_with_provider(client_auth_roots.into(), provider.clone())
                .build()
                .unwrap()
        }
        ClientAuth::No => WebPkiClientVerifier::no_client_auth(),
    };

    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[params.version])
        .unwrap()
        .with_client_cert_verifier(client_auth)
        .with_single_cert(params.key_type.get_chain(), params.key_type.get_key())
        .expect("bad certs/private key?");

    if resume == ResumptionParam::SessionId {
        cfg.session_storage = ServerSessionMemoryCache::new(128);
    } else if resume == ResumptionParam::Tickets {
        cfg.ticketer = Ticketer::new().unwrap();
    } else {
        cfg.session_storage = Arc::new(NoServerSessionStorage {});
    }

    cfg.max_fragment_size = max_fragment_size;
    cfg
}

fn make_client_config(
    params: &BenchmarkParam,
    clientauth: ClientAuth,
    resume: ResumptionParam,
) -> ClientConfig {
    let mut root_store = RootCertStore::empty();
    let mut rootbuf =
        io::BufReader::new(fs::File::open(params.key_type.path_for("ca.cert")).unwrap());
    root_store.add_parsable_certificates(
        rustls_pemfile::certs(&mut rootbuf).map(|result| result.unwrap()),
    );

    let cfg = ClientConfig::builder_with_provider(
        CryptoProvider {
            cipher_suites: vec![params.ciphersuite],
            ..provider::default_provider()
        }
        .into(),
    )
    .with_protocol_versions(&[params.version])
    .unwrap()
    .with_root_certificates(root_store);

    let mut cfg = if clientauth == ClientAuth::Yes {
        cfg.with_client_auth_cert(
            params.key_type.get_client_chain(),
            params.key_type.get_client_key(),
        )
        .unwrap()
    } else {
        cfg.with_no_client_auth()
    };

    if resume != ResumptionParam::No {
        cfg.resumption = Resumption::in_memory_sessions(128);
    } else {
        cfg.resumption = Resumption::disabled();
    }

    cfg
}

fn lookup_matching_benches(name: &str) -> Vec<&BenchmarkParam> {
    let r: Vec<&BenchmarkParam> = ALL_BENCHMARKS
        .iter()
        .filter(|params| {
            format!("{:?}", params.ciphersuite.suite()).to_lowercase() == name.to_lowercase()
        })
        .collect();

    if r.is_empty() {
        panic!("unknown suite {:?}", name);
    }

    r
}

#[derive(PartialEq, Clone, Copy)]
enum ClientAuth {
    No,
    Yes,
}

#[derive(PartialEq, Clone, Copy)]
enum ResumptionParam {
    No,
    SessionId,
    Tickets,
}

impl ResumptionParam {
    fn from_subcommand(cmd: &Command) -> Self {
        match cmd {
            Command::Handshake { .. } => Self::No,
            Command::HandshakeResume { .. } => Self::SessionId,
            Command::HandshakeTicket { .. } => Self::Tickets,
            _ => todo!("unhandled subcommand {cmd:?}"),
        }
    }

    fn label(&self) -> &'static str {
        match *self {
            Self::No => "no-resume",
            Self::SessionId => "sessionid",
            Self::Tickets => "tickets",
        }
    }
}

#[derive(Debug, Clone)]
struct Options {
    work_multiplier: f64,
}

impl Options {
    fn apply_work_multiplier(&self, work: u64) -> u64 {
        ((work as f64) * self.work_multiplier).round() as u64
    }
}

struct BenchmarkParam {
    key_type: KeyType,
    ciphersuite: rustls::SupportedCipherSuite,
    version: &'static rustls::SupportedProtocolVersion,
}

impl BenchmarkParam {
    const fn new(
        key_type: KeyType,
        ciphersuite: rustls::SupportedCipherSuite,
        version: &'static rustls::SupportedProtocolVersion,
    ) -> Self {
        Self {
            key_type,
            ciphersuite,
            version,
        }
    }
}

// copied from tests/api.rs
#[derive(PartialEq, Clone, Copy, Debug)]
enum KeyType {
    Rsa2048,
    EcdsaP256,
    EcdsaP384,
    Ed25519,
}

impl KeyType {
    fn path_for(&self, part: &str) -> String {
        match self {
            Self::Rsa2048 => format!("test-ca/rsa-2048/{}", part),
            Self::EcdsaP256 => format!("test-ca/ecdsa-p256/{}", part),
            Self::EcdsaP384 => format!("test-ca/ecdsa-p384/{}", part),
            Self::Ed25519 => format!("test-ca/eddsa/{}", part),
        }
    }

    fn get_chain(&self) -> Vec<CertificateDer<'static>> {
        rustls_pemfile::certs(&mut io::BufReader::new(
            fs::File::open(self.path_for("end.fullchain")).unwrap(),
        ))
        .map(|result| result.unwrap())
        .collect()
    }

    fn get_key(&self) -> PrivateKeyDer<'static> {
        rustls_pemfile::pkcs8_private_keys(&mut io::BufReader::new(
            fs::File::open(self.path_for("end.key")).unwrap(),
        ))
        .next()
        .unwrap()
        .unwrap()
        .into()
    }

    fn get_client_chain(&self) -> Vec<CertificateDer<'static>> {
        rustls_pemfile::certs(&mut io::BufReader::new(
            fs::File::open(self.path_for("client.fullchain")).unwrap(),
        ))
        .map(|result| result.unwrap())
        .collect()
    }

    fn get_client_key(&self) -> PrivateKeyDer<'static> {
        rustls_pemfile::pkcs8_private_keys(&mut io::BufReader::new(
            fs::File::open(self.path_for("client.key")).unwrap(),
        ))
        .next()
        .unwrap()
        .unwrap()
        .into()
    }
}

fn do_handshake_step(client: &mut ClientConnection, server: &mut ServerConnection) -> bool {
    if server.is_handshaking() || client.is_handshaking() {
        transfer(client, server, None);
        transfer(server, client, None);
        true
    } else {
        false
    }
}

fn do_handshake(client: &mut ClientConnection, server: &mut ServerConnection) {
    while do_handshake_step(client, server) {}
}

fn time<F>(mut f: F) -> f64
where
    F: FnMut(),
{
    let start = Instant::now();
    f();
    let end = Instant::now();
    duration_nanos(end.duration_since(start))
}

fn transfer<L, R, LS, RS>(left: &mut L, right: &mut R, expect_data: Option<usize>) -> f64
where
    L: DerefMut + Deref<Target = ConnectionCommon<LS>>,
    R: DerefMut + Deref<Target = ConnectionCommon<RS>>,
    LS: SideData,
    RS: SideData,
{
    let mut tls_buf = [0u8; 262144];
    let mut read_time = 0f64;
    let mut data_left = expect_data;
    let mut data_buf = [0u8; 8192];

    loop {
        let mut sz = 0;

        while left.wants_write() {
            let written = left
                .write_tls(&mut tls_buf[sz..].as_mut())
                .unwrap();
            if written == 0 {
                break;
            }

            sz += written;
        }

        if sz == 0 {
            return read_time;
        }

        let mut offs = 0;
        loop {
            let start = Instant::now();
            match right.read_tls(&mut tls_buf[offs..sz].as_ref()) {
                Ok(read) => {
                    right.process_new_packets().unwrap();
                    offs += read;
                }
                Err(err) => {
                    panic!("error on transfer {}..{}: {}", offs, sz, err);
                }
            }

            if let Some(left) = &mut data_left {
                loop {
                    let sz = match right.reader().read(&mut data_buf) {
                        Ok(sz) => sz,
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                        Err(err) => panic!("failed to read data: {}", err),
                    };

                    *left -= sz;
                    if *left == 0 {
                        break;
                    }
                }
            }

            let end = Instant::now();
            read_time += duration_nanos(end.duration_since(start));
            if sz == offs {
                break;
            }
        }
    }
}

fn duration_nanos(d: Duration) -> f64 {
    (d.as_secs() as f64) + f64::from(d.subsec_nanos()) / 1e9
}

static ALL_BENCHMARKS: &[BenchmarkParam] = &[
    #[cfg(all(feature = "tls12", not(feature = "fips")))]
    BenchmarkParam::new(
        KeyType::Rsa2048,
        cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        &rustls::version::TLS12,
    ),
    #[cfg(all(feature = "tls12", not(feature = "fips")))]
    BenchmarkParam::new(
        KeyType::EcdsaP256,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        &rustls::version::TLS12,
    ),
    #[cfg(feature = "tls12")]
    BenchmarkParam::new(
        KeyType::Rsa2048,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        &rustls::version::TLS12,
    ),
    #[cfg(feature = "tls12")]
    BenchmarkParam::new(
        KeyType::Rsa2048,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        &rustls::version::TLS12,
    ),
    #[cfg(feature = "tls12")]
    BenchmarkParam::new(
        KeyType::EcdsaP256,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        &rustls::version::TLS12,
    ),
    #[cfg(feature = "tls12")]
    BenchmarkParam::new(
        KeyType::EcdsaP384,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        &rustls::version::TLS12,
    ),
    #[cfg(not(feature = "fips"))]
    BenchmarkParam::new(
        KeyType::Rsa2048,
        cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        &rustls::version::TLS13,
    ),
    BenchmarkParam::new(
        KeyType::Rsa2048,
        cipher_suite::TLS13_AES_256_GCM_SHA384,
        &rustls::version::TLS13,
    ),
    BenchmarkParam::new(
        KeyType::Rsa2048,
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        &rustls::version::TLS13,
    ),
    BenchmarkParam::new(
        KeyType::EcdsaP256,
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        &rustls::version::TLS13,
    ),
    BenchmarkParam::new(
        KeyType::Ed25519,
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        &rustls::version::TLS13,
    ),
];

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
