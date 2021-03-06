use anyhow::{anyhow, Result};
use futures_util::{
    future::{self, BoxFuture},
    FutureExt,
};
use log::trace;
use rand::{Rng, SeedableRng};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    vec,
};

use trust_dns_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{Name, RData, RecordType},
    serialize::binary::{BinDecodable, BinEncodable},
};

use crate::{
    config::{Config, GeneralSettings},
    proxy::create_bounded_udp_socket,
};

macro_rules! random_get {
    ($v:expr) => {{
        use rand::random;
        let len = $v.len();
        let idx = random::<usize>() % len;
        $v.get(idx).expect("never reached!")
    }};
}
pub struct DnsClient {
    /// should be ipv4 addr
    pub remote_dns_servers: Vec<SocketAddr>,
    pub config: Config,
}

impl DnsClient {
    pub fn new(config: Config) -> DnsClient {
        let mut servers = Vec::new();
        if let Some(dns) = &config.dns {
            if let Some(server) = &dns.servers {
                let mut ss = Vec::new();
                for str in server {
                    let addr = match str.parse::<SocketAddr>() {
                        Ok(x) => x,
                        Err(err) => {
                            log::warn!("{} ip:{}", err, str);
                            continue;
                        }
                    };
                    ss.push(addr);
                }
                servers.extend_from_slice(&ss)
            }
        }

        DnsClient {
            remote_dns_servers: servers,
            config: config,
        }
    }
    pub fn new_query(host: &String, ty: RecordType) -> Message {
        let mut message = Message::new();
        let mut query = Query::new();
        let name = Name::from_str(&*host).expect("wrong host!");
        let mut random_generator = rand::rngs::StdRng::from_entropy();
        let random = random_generator.gen();
        query.set_name(name).set_query_type(ty);
        message.add_query(query);
        message.set_message_type(MessageType::Query);
        message.set_id(random);
        message.set_op_code(OpCode::Query);
        message.set_recursion_desired(true);
        message
    }

    /// domain string to ip
    pub async fn lookup(&self, host: &String) -> Result<Vec<IpAddr>> {
        let GeneralSettings {
            prefer_ipv6,
            use_ipv6,
            ..
        } = self.config.general;
        let mut tasks: Vec<BoxFuture<Result<Vec<IpAddr>>>> = Vec::new();
        match (use_ipv6, prefer_ipv6) {
            (true, true) => {
                // only wait ipv6 result
                let query = DnsClient::new_query(host, RecordType::AAAA);
                let server = random_get!(self.remote_dns_servers);
                let v = query.to_vec()?;
                let task = DnsClient::do_lookup(v, &*host, server).boxed();
                tasks.push(task);
            }
            (true, false) => {
                // wait the first result
                let server = random_get!(self.remote_dns_servers);
                let query = DnsClient::new_query(&host, RecordType::A);
                let v = query.to_vec()?;
                let task = DnsClient::do_lookup(v, &*host, server).boxed();
                tasks.push(task);
                let query = DnsClient::new_query(&host, RecordType::AAAA);
                let v = query.to_vec()?;
                let task = DnsClient::do_lookup(v, &*host, server).boxed();
                tasks.push(task);
            }
            (false, ..) => {
                // don't use ipv6
                // just use ipv4
                let server = random_get!(self.remote_dns_servers);
                let query = DnsClient::new_query(&host, RecordType::A);
                let v = query.to_vec()?;
                let task = DnsClient::do_lookup(v, &*host, server).boxed();
                tasks.push(task);
            }
        };
        let mut ips = Vec::new();
        for mut res in future::join_all(tasks).await {
            match res {
                Ok(ref mut x) => {
                    ips.append(x);
                }
                Err(err) => return Err(anyhow!("lookup failed error {}", err)),
            }
        }
        Ok(ips)
    }
    pub async fn do_lookup(
        request: Vec<u8>,
        host: &str,
        server: &SocketAddr,
    ) -> Result<Vec<IpAddr>> {
        trace!("lookup {} on DNS server {}", host, &server);
        let socket = match server {
            SocketAddr::V4(_v4) => {
                // let bind_addr = get_default_ipv4_gateway()?;
                let bind_addr = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
                create_bounded_udp_socket(bind_addr)?
            }
            SocketAddr::V6(_v6) => {
                // let bind_addr = get_default_ipv6_gateway()?;
                let bind_addr = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0));
                create_bounded_udp_socket(bind_addr)?
            }
        };
        match socket.send_to(&*request, server).await {
            Ok(..) => {
                let mut buf = vec![0u8; 512];
                match socket.recv_from(&mut buf).await {
                    Ok((n, ..)) => {
                        let message = Message::from_bytes(&buf[..n])?;
                        if message.response_code() != ResponseCode::NoError {
                            return Err(anyhow!(
                                "dns lookup response indicate failed {}",
                                message.response_code()
                            ));
                        }
                        let anwsers = message.answers();
                        let mut ips = Vec::new();
                        for anwser in anwsers {
                            let rdata = anwser.rdata();
                            match rdata {
                                RData::A(ip) => ips.push(IpAddr::V4(ip.clone())),
                                RData::AAAA(ipv6) => ips.push(IpAddr::V6(ipv6.clone())),
                                _ => {}
                            };
                        }
                        return Ok(ips);
                    }
                    Err(err) => return Err(anyhow!("error when recv from {}", err)),
                }
            }
            Err(err) => return Err(anyhow!("error when send to {}", err)),
        };
    }
}

#[tokio::test]
async fn lookup_test() {
    use tokio::net::UdpSocket;

    let host = "www.baidu.com".to_string();
    let query = DnsClient::new_query(&host, RecordType::A);
    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let target = "114.114.114.114:53".parse::<SocketAddr>().unwrap();
    socket
        .send_to(&*query.to_bytes().unwrap(), target)
        .await
        .unwrap();
    let mut buf = [0; 512];
    let (n, _) = socket.recv_from(&mut buf).await.unwrap();
    let message = Message::from_bytes(&buf[..n]).unwrap();
    let answers = message.answers();
    let mut ips = Vec::new();
    for answer in answers {
        let rdata = answer.rdata();
        match rdata {
            RData::A(v4) => {
                ips.push(v4.to_string());
            }
            RData::AAAA(v6) => {
                ips.push(v6.to_string());
            }
            _ => {}
        }
    }
    println!("{:?}", ips);
}
