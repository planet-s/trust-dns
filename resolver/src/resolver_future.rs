// Copyright 2015-2017 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Structs for creating and using a ResolverFuture
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use futures::Future;
use tokio_core::reactor::Handle;
use trust_dns_proto::op::Message;
use trust_dns_proto::{BasicDnsHandle, DnsHandle, RetryDnsHandle};
#[cfg(feature = "dnssec")]
use trust_dns_proto::SecureDnsHandle;
use trust_dns_proto::rr::{IntoName, Name, RecordType};

use config::{ResolverConfig, ResolverOpts};
use error::*;
use lookup_state::CachingClient;
use name_server_pool::{NameServerPool, StandardConnection};
use lookup_ip::{InnerLookupIpFuture, LookupIpFuture};
use dns_lru::DnsLru;
use lookup::{self, Lookup};
use lookup::{InnerLookupFuture, LookupEither, LookupFuture};
use hosts::{parse_literal_ip, Hosts};

/// Root Handle to communicate with the ResolverFuture
///
/// This can be used directly to perform queries. See [`trust_dns_proto::SecureClientHandle`] for
///  a DNSSEc chain validator.
#[derive(Clone)]
pub struct BasicResolverHandle {
    message_sender: BasicDnsHandle<ResolveError>,
}

impl BasicResolverHandle {
    pub(crate) fn new(dns_handle: BasicDnsHandle<ResolveError>) -> Self {
        BasicResolverHandle {
            message_sender: dns_handle,
        }
    }
}

impl DnsHandle for BasicResolverHandle {
    type Error = ResolveError;

    fn send(&mut self, message: Message) -> Box<Future<Item = Message, Error = Self::Error>> {
        Box::new(
            self.message_sender
                .send(message)
                .map_err(ResolveError::from),
        )
    }
}

/// A Resolver for DNS records.
pub struct ResolverFuture {
    config: ResolverConfig,
    options: ResolverOpts,
    client_cache: CachingClient<LookupEither<BasicResolverHandle, StandardConnection>>,
    hosts: Option<Arc<Hosts>>,
}

macro_rules! lookup_fn {
    ($p:ident, $f:ty, $r:path) => {
/// Performs a lookup for the associated type.
///
/// *hint* queries that end with a '.' are fully qualified names and are cheaper lookups
///
/// # Arguments
///
/// * `query` - a string which parses to a domain name, failure to parse will return an error
pub fn $p<N: IntoName>(&self, query: N) -> $f {
    let name = match query.into_name() {
        Ok(name) => name,
        Err(err) => {
            return InnerLookupFuture::error(self.client_cache.clone(), err).into();
        }
    };

    self.inner_lookup(name, $r).into()
}
    };
    ($p:ident, $f:ty, $r:path, $t:ty) => {
/// Performs a lookup for the associated type.
///
/// # Arguments
///
/// * `query` - a type which can be converted to `Name` via `From`.
pub fn $p(&self, query: $t) -> $f {
    let name = Name::from(query);
    self.inner_lookup(name, $r).into()
}
    };
}

impl ResolverFuture {
    /// Construct a new ResolverFuture with the associated Client and configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - configuration, name_servers, etc. for the Resolver
    /// * `options` - basic lookup options for the resolver
    /// * `reactor` - the [`tokio_core::Core`] to use with this future
    pub fn new(config: ResolverConfig, options: ResolverOpts, reactor: &Handle) -> Self {
        let lru = Arc::new(Mutex::new(DnsLru::new(options.cache_size)));

        Self::with_cache(config, options, lru, reactor)
    }

    /// Construct a new ResolverFuture with the associated Client and configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - configuration, name_servers, etc. for the Resolver
    /// * `options` - basic lookup options for the resolver
    /// * `lru` - the cache to be used with the resolver
    /// * `reactor` - the [`tokio_core::Core`] to use with this future
    pub(crate) fn with_cache(
        config: ResolverConfig,
        options: ResolverOpts,
        lru: Arc<Mutex<DnsLru>>,
        reactor: &Handle,
    ) -> Self {
        let pool = NameServerPool::<BasicResolverHandle, StandardConnection>::from_config(
            &config,
            &options,
            reactor,
        );
        let either;
        let client = RetryDnsHandle::new(pool.clone(), options.attempts);
        if options.validate {
            #[cfg(feature = "dnssec")]
            {
                either = LookupEither::Secure(SecureDnsHandle::new(client));
            }

            #[cfg(not(feature = "dnssec"))]
            {
                // TODO: should this just be a panic, or a pinned error?
                warn!("validate option is only available with 'dnssec' feature");
                either = LookupEither::Retry(client);
            }
        } else {
            either = LookupEither::Retry(client);
        }

        let hosts = if options.use_hosts_file {
            Some(Arc::new(Hosts::new()))
        } else {
            None
        };

        ResolverFuture {
            config,
            options,
            client_cache: CachingClient::with_cache(lru, either),
            hosts: hosts,
        }
    }

    /// Constructs a new Resolver with the system configuration.
    ///
    /// This will use `/etc/resolv.conf` on Unix OSes and the registry on Windows.
    #[cfg(any(unix, target_os = "windows"))]
    pub fn from_system_conf(reactor: &Handle) -> ResolveResult<Self> {
        let (config, options) = super::system_conf::read_system_conf()?;
        Ok(Self::new(config, options, reactor))
    }

    fn push_name(name: Name, names: &mut Vec<Name>) {
        if !names.contains(&name) {
            names.push(name);
        }
    }

    fn build_names(&self, name: Name) -> Vec<Name> {
        // if it's fully qualified, we can short circuit the lookup logic
        if name.is_fqdn() {
            vec![name]
        } else {
            // Otherwise we have to build the search list
            // Note: the vec is built in reverse order of precedence, for stack semantics
            let mut names =
                Vec::<Name>::with_capacity(1 /*FQDN*/ + 1 /*DOMAIN*/ + self.config.search().len());

            // if not meeting ndots, we always do the raw name in the final lookup, or it's a localhost...
            let raw_name_first: bool = name.num_labels() as usize > self.options.ndots || name.is_localhost();

            // if not meeting ndots, we always do the raw name in the final lookup
            if !raw_name_first {
                names.push(name.clone());
            }

            for search in self.config.search().iter().rev() {
                let name_search = name.clone().append_domain(search);
                Self::push_name(name_search, &mut names);
            }

            if let Some(domain) = self.config.domain() {
                let name_search = name.clone().append_domain(domain);
                Self::push_name(name_search, &mut names);
            }

            // this is the direct name lookup
            if raw_name_first {
                // adding the name as though it's an FQDN for lookup
                names.push(name.clone());
            }

            names
        }
    }

    /// Generic lookup for any RecordType
    ///
    /// *WARNING* this interface may change in the future, see if one of the specializations would be better.
    ///
    /// # Arguments
    ///
    /// * `name` - name of the record to lookup, if name is not a valid domain name, an error will be returned
    /// * `record_type` - type of record to lookup, all RecordData responses will be filtered to this type
    ///
    /// # Returns
    ///
    //  A future for the returned Lookup RData
    pub fn lookup<N: IntoName>(&self, name: N, record_type: RecordType) -> LookupFuture {
        let name = match name.into_name() {
            Ok(name) => name,
            Err(err) => {
                return InnerLookupFuture::error(self.client_cache.clone(), err);
            }
        };

        self.inner_lookup(name, record_type)
    }

    fn inner_lookup(&self, name: Name, record_type: RecordType) -> LookupFuture {
        let names = self.build_names(name);
        LookupFuture::lookup(names, record_type, self.client_cache.clone())
    }

    /// Performs a dual-stack DNS lookup for the IP for the given hostname.
    ///
    /// See the configuration and options parameters for controlling the way in which A(Ipv4) and AAAA(Ipv6) lookups will be performed. For the least expensive query a fully-qualified-domain-name, FQDN, which ends in a final `.`, e.g. `www.example.com.`, will only issue one query. Anything else will always incur the cost of querying the `ResolverConfig::domain` and `ResolverConfig::search`.
    ///
    /// # Arguments
    /// * `host` - string hostname, if this is an invalid hostname, an error will be returned.
    pub fn lookup_ip(&self, host: &str) -> LookupIpFuture {
        // if host is a ip address, return directly.
        if let Some(addr) = parse_literal_ip(host) {
            return InnerLookupIpFuture::ok(
                self.client_cache.clone(),
                Lookup::new(Arc::new(vec![addr])),
            );
        }

        let name = match Name::from_str(host) {
            Ok(name) => name,
            Err(err) => {
                return InnerLookupIpFuture::error(self.client_cache.clone(), err);
            }
        };

        let names = self.build_names(name);
        let hosts = if let Some(ref hosts) = self.hosts {
            Some(Arc::clone(hosts))
        } else {
            None
        };
        LookupIpFuture::lookup(
            names,
            self.options.ip_strategy,
            self.client_cache.clone(),
            hosts,
        )
    }

    /// Performs a DNS lookup for an SRV record for the specified service type and protocol at the given name.
    ///
    /// This is a convenience method over `lookup_srv`, it combines the service, protocol and name into a single name: `_service._protocol.name`.
    ///
    /// # Arguments
    ///
    /// * `service` - service to lookup, e.g. ldap or http
    /// * `protocol` - wire protocol, e.g. udp or tcp
    /// * `name` - zone or other name at which the service is located.
    pub fn lookup_service(
        &self,
        service: &str,
        protocol: &str,
        name: &str,
    ) -> lookup::SrvLookupFuture {
        let name = format!("_{}._{}.{}", service, protocol, name);
        self.srv_lookup(name)
    }

    lookup_fn!(
        reverse_lookup,
        lookup::ReverseLookupFuture,
        RecordType::PTR,
        IpAddr
    );
    lookup_fn!(ipv4_lookup, lookup::Ipv4LookupFuture, RecordType::A);
    lookup_fn!(ipv6_lookup, lookup::Ipv6LookupFuture, RecordType::AAAA);
    lookup_fn!(mx_lookup, lookup::MxLookupFuture, RecordType::MX);
    lookup_fn!(srv_lookup, lookup::SrvLookupFuture, RecordType::SRV);
    lookup_fn!(txt_lookup, lookup::TxtLookupFuture, RecordType::TXT);
}

#[cfg(test)]
mod tests {
    extern crate tokio_core;

    use std::net::*;
    use std::str::FromStr;

    use self::tokio_core::reactor::Core;
    use trust_dns_proto::error::ProtoErrorKind;

    use config::{LookupIpStrategy, NameServerConfig};

    use super::*;

    #[test]
    fn test_lookup() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts::default(),
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("www.example.com."))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert_eq!(
                    address,
                    IpAddr::V6(Ipv6Addr::new(
                        0x2606,
                        0x2800,
                        0x220,
                        0x1,
                        0x248,
                        0x1893,
                        0x25c8,
                        0x1946,
                    ))
                );
            }
        }
    }

    #[test]
    fn test_ip_lookup() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts::default(),
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("10.1.0.2"))
            .expect("failed to run lookup");

        assert_eq!(
            Some(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 2))),
            response.iter().next()
        );

        let response = io_loop
            .run(resolver.lookup_ip("2606:2800:220:1:248:1893:25c8:1946"))
            .expect("failed to run lookup");

        assert_eq!(
            Some(IpAddr::V6(Ipv6Addr::new(
                0x2606,
                0x2800,
                0x220,
                0x1,
                0x248,
                0x1893,
                0x25c8,
                0x1946,
            ))),
            response.iter().next()
        );
    }

    #[test]
    #[ignore] // these appear to not work on travis
    fn test_sec_lookup() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts {
                validate: true,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("www.example.com."))
            .expect("failed to run lookup");

        // TODO: this test is flaky, sometimes 1 is returned, sometimes 2...
        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert_eq!(
                    address,
                    IpAddr::V6(Ipv6Addr::new(
                        0x2606,
                        0x2800,
                        0x220,
                        0x1,
                        0x248,
                        0x1893,
                        0x25c8,
                        0x1946,
                    ))
                );
            }
        }
    }

    #[test]
    #[ignore] // these appear to not work on travis
    fn test_sec_lookup_fails() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts {
                validate: true,
                ip_strategy: LookupIpStrategy::Ipv4Only,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        // needs to be a domain that exists, but is not signed (eventually this will be)
        let name = Name::from_str("www.trust-dns.org.").unwrap();
        let response = io_loop.run(resolver.lookup_ip("www.trust-dns.org."));

        assert!(response.is_err());
        let error = response.unwrap_err();

        assert_eq!(
            error.kind(),
            &ResolveErrorKind::Proto(ProtoErrorKind::RrsigsNotPresent(name, RecordType::A))
        );
    }

    #[test]
    #[ignore]
    #[cfg(any(unix, target_os = "windows"))]
    fn test_system_lookup() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::from_system_conf(&io_loop.handle()).unwrap();

        let response = io_loop
            .run(resolver.lookup_ip("www.example.com."))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 2);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert_eq!(
                    address,
                    IpAddr::V6(Ipv6Addr::new(
                        0x2606,
                        0x2800,
                        0x220,
                        0x1,
                        0x248,
                        0x1893,
                        0x25c8,
                        0x1946,
                    ))
                );
            }
        }
    }

    #[test]
    #[ignore]
    // these appear to not work on travis, test on macos with `10.1.0.104  a.com`
    #[cfg(unix)]
    fn test_hosts_lookup() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::from_system_conf(&io_loop.handle()).unwrap();

        let response = io_loop
            .run(resolver.lookup_ip("a.com"))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(10, 1, 0, 104)));
            } else {
                assert!(false, "failed to run lookup");
            }
        }
    }

    #[test]
    fn test_fqdn() {
        let domain = Name::from_str("incorrect.example.com.").unwrap();
        let search = vec![
            Name::from_str("bad.example.com.").unwrap(),
            Name::from_str("wrong.example.com.").unwrap(),
        ];
        let name_servers: Vec<NameServerConfig> =
            ResolverConfig::default().name_servers().to_owned();

        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::from_parts(Some(domain), search, name_servers),
            ResolverOpts {
                ip_strategy: LookupIpStrategy::Ipv4Only,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("www.example.com."))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert!(false, "should only be looking up IPv4");
            }
        }
    }

    #[test]
    fn test_ndots() {
        let domain = Name::from_str("incorrect.example.com.").unwrap();
        let search = vec![
            Name::from_str("bad.example.com.").unwrap(),
            Name::from_str("wrong.example.com.").unwrap(),
        ];
        let name_servers: Vec<NameServerConfig> =
            ResolverConfig::default().name_servers().to_owned();

        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::from_parts(Some(domain), search, name_servers),
            ResolverOpts {
                // our name does have 2, the default should be fine, let's just narrow the test criteria a bit.
                ndots: 2,
                ip_strategy: LookupIpStrategy::Ipv4Only,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        // notice this is not a FQDN, no trailing dot.
        let response = io_loop
            .run(resolver.lookup_ip("www.example.com"))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert!(false, "should only be looking up IPv4");
            }
        }
    }

    #[test]
    fn test_large_ndots() {
        let domain = Name::from_str("incorrect.example.com.").unwrap();
        let search = vec![
            Name::from_str("bad.example.com.").unwrap(),
            Name::from_str("wrong.example.com.").unwrap(),
        ];
        let name_servers: Vec<NameServerConfig> =
            ResolverConfig::default().name_servers().to_owned();

        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::from_parts(Some(domain), search, name_servers),
            ResolverOpts {
                // matches kubernetes default
                ndots: 5,
                ip_strategy: LookupIpStrategy::Ipv4Only,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        // notice this is not a FQDN, no trailing dot.
        let response = io_loop
            .run(resolver.lookup_ip("www.example.com"))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert!(false, "should only be looking up IPv4");
            }
        }
    }

    #[test]
    fn test_domain_search() {
        // domain is good now, shoudl be combined with the name to form www.example.com
        let domain = Name::from_str("example.com.").unwrap();
        let search = vec![
            Name::from_str("bad.example.com.").unwrap(),
            Name::from_str("wrong.example.com.").unwrap(),
        ];
        let name_servers: Vec<NameServerConfig> =
            ResolverConfig::default().name_servers().to_owned();

        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::from_parts(Some(domain), search, name_servers),
            ResolverOpts {
                ip_strategy: LookupIpStrategy::Ipv4Only,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        // notice no dots, should not trigger ndots rule
        let response = io_loop
            .run(resolver.lookup_ip("www"))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert!(false, "should only be looking up IPv4");
            }
        }
    }

    #[test]
    fn test_search_list() {
        let domain = Name::from_str("incorrect.example.com.").unwrap();
        let search = vec![
            // let's skip one search domain to test the loop...
            Name::from_str("bad.example.com.").unwrap(),
            // this should combine with the search name to form www.example.com
            Name::from_str("example.com.").unwrap(),
        ];
        let name_servers: Vec<NameServerConfig> =
            ResolverConfig::default().name_servers().to_owned();

        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::from_parts(Some(domain), search, name_servers),
            ResolverOpts {
                ip_strategy: LookupIpStrategy::Ipv4Only,
                ..ResolverOpts::default()
            },
            &io_loop.handle(),
        );

        // notice no dots, should not trigger ndots rule
        let response = io_loop
            .run(resolver.lookup_ip("www"))
            .expect("failed to run lookup");

        assert_eq!(response.iter().count(), 1);
        for address in response.iter() {
            if address.is_ipv4() {
                assert_eq!(address, IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)));
            } else {
                assert!(false, "should only be looking up IPv4");
            }
        }
    }

    #[test]
    fn test_idna() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts::default(),
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("??????.icom.museum."))
            .expect("failed to run lookup");

        // we just care that the request succeeded, not about the actual content
        //   it's not certain that the ip won't change.
        assert!(response.iter().next().is_some());
    }

    #[test]
    fn test_localhost_ipv4() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts{ip_strategy: LookupIpStrategy::Ipv4thenIpv6,.. ResolverOpts::default()},
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("localhost"))
            .expect("failed to run lookup");

        let mut iter = response.iter();
        assert_eq!(iter.next().expect("no A"), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn test_localhost_ipv6() {
        let mut io_loop = Core::new().unwrap();
        let resolver = ResolverFuture::new(
            ResolverConfig::default(),
            ResolverOpts{ip_strategy: LookupIpStrategy::Ipv6thenIpv4,.. ResolverOpts::default()},
            &io_loop.handle(),
        );

        let response = io_loop
            .run(resolver.lookup_ip("localhost"))
            .expect("failed to run lookup");

        let mut iter = response.iter();
        assert_eq!(
            iter.next().expect("no AAAA"),
            IpAddr::V6(Ipv6Addr::new(
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                1,
            ))
        );
    }
}
