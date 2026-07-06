//! mDNS advertisement (`_nvstream._tcp.local.`) so Moonlight clients find
//! this host without needing to add it by IP manually.

use std::collections::HashMap;
use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceInfo};

const SERVICE_TYPE: &str = "_nvstream._tcp.local.";

/// Advertises the redfog moonlight-server over mDNS until dropped.
pub struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Discovery {
    pub fn spawn(hostname: &str, address: IpAddr, port: u16) -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("failed to start mdns daemon: {e}"))?;

        let instance_name = format!("{hostname}-redfog");
        let host_fqdn = format!("{hostname}.local.");
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_fqdn,
            address,
            port,
            HashMap::<String, String>::new(),
        )
        .map_err(|e| format!("failed to build mdns service info: {e}"))?;

        let fullname = service.get_fullname().to_string();
        daemon
            .register(service)
            .map_err(|e| format!("failed to register mdns service: {e}"))?;

        tracing::info!("advertising redfog-server over mDNS as {fullname}");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Err(e) = self.daemon.unregister(&self.fullname) {
            tracing::debug!("failed to unregister mdns service {}: {e:?}", self.fullname);
        }
        let _ = self.daemon.shutdown();
    }
}
