//! Native Cloudflare provisioning. The API token exists only for this command;
//! only the connector token is returned to the OS-keyring writer. Never replace
//! a conflicting DNS record or silently repoint an existing tunnel.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use zeroize::Zeroizing;
const ERROR:&str="Cloudflare setup could not be verified. Retry the same setup; no existing DNS record or foreign tunnel was replaced";
pub struct Provisioner {
    client: reqwest::Client,
    base: String,
}
impl Provisioner {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            client: reqwest::Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|_| ERROR)?,
            base: "https://api.cloudflare.com/client/v4".into(),
        })
    }
    async fn request(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        payload: Option<Value>,
    ) -> Result<Value, String> {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
        if let Some(p) = payload {
            req = req.json(&p);
        }
        let mut response = req.send().await.map_err(|_| ERROR)?;
        if !response.status().is_success() {
            return Err(match response.status().as_u16(){401=>"Cloudflare rejected the API token",403=>"The API token needs Tunnel Edit and DNS Edit permissions for the selected account and zone",429=>"Cloudflare rate limited setup; retry later",_=>ERROR}.into());
        }
        if response.content_length().is_some_and(|n| n > 512 * 1024) {
            return Err(ERROR.into());
        }
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(c) = response.chunk().await.map_err(|_| ERROR)? {
            if bytes.len() + c.len() > 512 * 1024 {
                return Err(ERROR.into());
            }
            bytes.extend_from_slice(&c);
        }
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
        if value["success"] != true
            || value["result_info"]["total_pages"]
                .as_u64()
                .is_some_and(|n| n > 1)
        {
            return Err(ERROR.into());
        }
        value.get_mut("result").map(Value::take).ok_or(ERROR.into())
    }
    pub async fn provision(
        &self,
        token: Zeroizing<Vec<u8>>,
        account: &str,
        zone: &str,
        host: &str,
        port: u16,
        environment: &str,
    ) -> Result<Zeroizing<Vec<u8>>, String> {
        let token = std::str::from_utf8(&token).map_err(|_| ERROR)?;
        if token.len() < 20
            || token.len() > 1024
            || !token.bytes().all(|b| b.is_ascii_graphic())
            || !id(account)
            || !id(zone)
            || port == 0
        {
            return Err("Invalid scoped API token, account or zone".into());
        }
        crate::named_tunnel::Config {
            hostname: host.into(),
            account: "a".repeat(64),
            origin: ([127, 0, 0, 1], port).into(),
        }
        .validate()?;
        const GET: reqwest::Method = reqwest::Method::GET;
        const POST: reqwest::Method = reqwest::Method::POST;
        const PUT: reqwest::Method = reqwest::Method::PUT;
        let z = self
            .request(token, GET, &format!("/zones/{zone}"), None)
            .await?;
        let zone_name = z["name"].as_str().ok_or(ERROR)?;
        if z["id"] != zone
            || z["account"]["id"] != account
            || z["status"] != "active"
            || !host.ends_with(&format!(".{zone_name}"))
        {
            return Err(
                "Choose a hostname in the selected active Cloudflare zone and account".into(),
            );
        }
        // Check conflicts before allocating anything. Retried requests can reuse
        // only a tunnel with this installation-specific name and exact ingress.
        let dns = self
            .request(
                token,
                GET,
                &format!("/zones/{zone}/dns_records?name={host}&per_page=100"),
                None,
            )
            .await?;
        let dns = dns.as_array().ok_or(ERROR)?;
        if dns.len() > 1
            || dns
                .first()
                .is_some_and(|d| d["name"] != host || d["type"] != "CNAME" || d["proxied"] != true)
        {
            return Err(
                "The hostname already has an incompatible DNS record; it was retained".into(),
            );
        }
        let name = format!(
            "remodex-native-{:x}",
            Sha256::digest(format!("{environment}\0{host}"))
        );
        let tunnels = self
            .request(
                token,
                GET,
                &format!(
                    "/accounts/{account}/cfd_tunnel?is_deleted=false&name={name}&per_page=100"
                ),
                None,
            )
            .await?;
        let rows = tunnels.as_array().ok_or(ERROR)?;
        if rows.len() > 1
            || rows.first().is_some_and(|t| {
                t["name"] != name || t["remote_config"] != true || t["tun_type"] != "cfd_tunnel"
            })
        {
            return Err(ERROR.into());
        }
        let ingress = json!([{ "hostname":host,"service":format!("http://127.0.0.1:{port}"),"originRequest":{}},{"service":"http_status:404"}]);
        let (tunnel, created) = if let Some(t) = rows.first() {
            (t.clone(), false)
        } else {
            if !dns.is_empty() {
                return Err(
                    "The hostname belongs to another tunnel; its DNS record was retained".into(),
                );
            }
            (
                self.request(
                    token,
                    POST,
                    &format!("/accounts/{account}/cfd_tunnel"),
                    Some(json!({"name":name,"config_src":"cloudflare"})),
                )
                .await?,
                true,
            )
        };
        let tunnel_id = tunnel["id"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
            .ok_or(ERROR)?;
        let route = format!("/accounts/{account}/cfd_tunnel/{tunnel_id}/configurations");
        if created {
            self.request(
                token,
                PUT,
                &route,
                Some(json!({"config":{"ingress":ingress}})),
            )
            .await?;
        } else {
            let config = self.request(token, GET, &route, None).await?;
            if config["config"]["ingress"] != ingress {
                // A previous create may have committed before its response or
                // route update was lost. Only fill an unused, unconfigured
                // installation-named tunnel; never overwrite any existing route.
                let blank = config["config"].as_object().is_some_and(|c| {
                    c.iter().all(|(k, v)| match k.as_str() {
                        "ingress" => v.as_array().is_some_and(Vec::is_empty),
                        "originRequest" => v.as_object().is_some_and(serde_json::Map::is_empty),
                        _ => false,
                    })
                });
                if !dns.is_empty() || !blank {
                    return Err("The existing tunnel has different routing; use a different hostname or restore its original routing in Cloudflare".into());
                }
                self.request(
                    token,
                    PUT,
                    &route,
                    Some(json!({"config":{"ingress":ingress}})),
                )
                .await?;
            }
        }
        let target = format!("{tunnel_id}.cfargotunnel.com");
        if let Some(d) = dns.first() {
            if d["content"] != target {
                return Err(
                    "The hostname points to another tunnel; its DNS record was retained".into(),
                );
            }
        } else {
            self.request(
                token,
                POST,
                &format!("/zones/{zone}/dns_records"),
                Some(json!({"type":"CNAME","name":host,"content":target,"proxied":true})),
            )
            .await?;
        }
        let mut result = self
            .request(
                token,
                GET,
                &format!("/accounts/{account}/cfd_tunnel/{tunnel_id}/token"),
                None,
            )
            .await?;
        let Value::String(secret) = result.take() else {
            return Err(ERROR.into());
        };
        let secret = Zeroizing::new(secret.into_bytes());
        if secret.is_empty() || secret.len() > 16384 || !secret.iter().all(|b| b.is_ascii_graphic())
        {
            return Err(ERROR.into());
        }
        // No automatic deletion on a timeout: the API may have committed it.
        // Retrying verifies the exact owned route instead of duplicating it.
        Ok(secret)
    }
}
fn id(v: &str) -> bool {
    v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::{OriginalUri, State},
        http::{Method, StatusCode},
        routing::any,
        Json, Router,
    };
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    type Step = (Method, String, Option<Value>, u16, Value);
    async fn fixture(
        steps: Vec<Step>,
    ) -> (
        Provisioner,
        Arc<Mutex<VecDeque<Step>>>,
        tokio::task::JoinHandle<()>,
    ) {
        async fn handler(
            State(queue): State<Arc<Mutex<VecDeque<Step>>>>,
            OriginalUri(uri): OriginalUri,
            method: Method,
            body: Bytes,
        ) -> (StatusCode, Json<Value>) {
            let (expected, path, payload, status, response) = queue
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected API request");
            assert_eq!(method, expected);
            assert_eq!(uri.to_string(), path);
            if let Some(payload) = payload {
                assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), payload);
            } else {
                assert!(body.is_empty());
            }
            (StatusCode::from_u16(status).unwrap(), Json(response))
        }
        let queue = Arc::new(Mutex::new(VecDeque::from(steps)));
        let router = Router::new()
            .fallback(any(handler))
            .with_state(queue.clone());
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
        (
            Provisioner {
                client: reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(2))
                    .build()
                    .unwrap(),
                base: format!("http://{address}"),
            },
            queue,
            task,
        )
    }
    fn response(result: Value) -> Value {
        json!({"success":true,"result":result})
    }
    fn token() -> Zeroizing<Vec<u8>> {
        Zeroizing::new(b"fixture-scoped-api-token-only".to_vec())
    }
    #[tokio::test]
    async fn provision_and_retry_use_exact_phone_route_without_duplicate_resources() {
        let account = "a".repeat(32);
        let zone = "b".repeat(32);
        let host = "phone.example.com";
        let environment = "fixture-installation";
        let name = format!(
            "remodex-native-{:x}",
            Sha256::digest(format!("{environment}\0{host}"))
        );
        let tunnel_id = "00000000-0000-4000-8000-000000000001";
        let zone_info =
            json!({"id":zone,"account":{"id":account},"name":"example.com","status":"active"});
        let ingress = json!([{"hostname":host,"service":"http://127.0.0.1:12005","originRequest":{}},{"service":"http_status:404"}]);
        let tunnel =
            json!({"id":tunnel_id,"name":name,"remote_config":true,"tun_type":"cfd_tunnel"});
        let dns = json!({"type":"CNAME","name":host,"content":format!("{tunnel_id}.cfargotunnel.com"),"proxied":true});
        let mut steps = vec![
            (
                Method::GET,
                format!("/zones/{zone}"),
                None,
                200,
                response(zone_info.clone()),
            ),
            (
                Method::GET,
                format!("/zones/{zone}/dns_records?name={host}&per_page=100"),
                None,
                200,
                response(json!([])),
            ),
            (
                Method::GET,
                format!("/accounts/{account}/cfd_tunnel?is_deleted=false&name={name}&per_page=100"),
                None,
                200,
                response(json!([])),
            ),
            (
                Method::POST,
                format!("/accounts/{account}/cfd_tunnel"),
                Some(json!({"name":name,"config_src":"cloudflare"})),
                200,
                response(tunnel.clone()),
            ),
            (
                Method::PUT,
                format!("/accounts/{account}/cfd_tunnel/{tunnel_id}/configurations"),
                Some(json!({"config":{"ingress":ingress}})),
                200,
                response(json!({})),
            ),
            (
                Method::POST,
                format!("/zones/{zone}/dns_records"),
                Some(dns.clone()),
                200,
                response(json!({"id":"fixture-dns"})),
            ),
            (
                Method::GET,
                format!("/accounts/{account}/cfd_tunnel/{tunnel_id}/token"),
                None,
                200,
                response(json!("fixture-connector-only")),
            ),
        ];
        steps.extend([
            (
                Method::GET,
                format!("/zones/{zone}"),
                None,
                200,
                response(zone_info),
            ),
            (
                Method::GET,
                format!("/zones/{zone}/dns_records?name={host}&per_page=100"),
                None,
                200,
                response(json!([dns])),
            ),
            (
                Method::GET,
                format!("/accounts/{account}/cfd_tunnel?is_deleted=false&name={name}&per_page=100"),
                None,
                200,
                response(json!([tunnel])),
            ),
            (
                Method::GET,
                format!("/accounts/{account}/cfd_tunnel/{tunnel_id}/configurations"),
                None,
                200,
                response(json!({"config":{"ingress":ingress}})),
            ),
            (
                Method::GET,
                format!("/accounts/{account}/cfd_tunnel/{tunnel_id}/token"),
                None,
                200,
                response(json!("fixture-connector-only")),
            ),
        ]);
        let (api, queue, task) = fixture(steps).await;
        for _ in 0..2 {
            assert_eq!(
                &*api
                    .provision(token(), &account, &zone, host, 12005, environment)
                    .await
                    .unwrap(),
                b"fixture-connector-only"
            );
        }
        assert!(queue.lock().unwrap().is_empty());
        task.abort();
    }
    #[tokio::test]
    async fn interrupted_create_repairs_only_an_empty_unused_owned_route() {
        let account = "a".repeat(32);
        let zone = "b".repeat(32);
        let host = "phone.example.com";
        let environment = "fixture";
        let name = format!(
            "remodex-native-{:x}",
            Sha256::digest(format!("{environment}\0{host}"))
        );
        let tid = "00000000-0000-4000-8000-000000000001";
        let prefix = || {
            vec![
                (
                    Method::GET,
                    format!("/zones/{zone}"),
                    None,
                    200,
                    response(
                        json!({"id":zone,"account":{"id":account},"name":"example.com","status":"active"}),
                    ),
                ),
                (
                    Method::GET,
                    format!("/zones/{zone}/dns_records?name={host}&per_page=100"),
                    None,
                    200,
                    response(json!([])),
                ),
                (
                    Method::GET,
                    format!(
                        "/accounts/{account}/cfd_tunnel?is_deleted=false&name={name}&per_page=100"
                    ),
                    None,
                    200,
                    response(
                        json!([{"id":tid,"name":name,"remote_config":true,"tun_type":"cfd_tunnel"}]),
                    ),
                ),
            ]
        };
        let ingress = json!([{"hostname":host,"service":"http://127.0.0.1:12005","originRequest":{}},{"service":"http_status:404"}]);
        let mut steps = prefix();
        steps.extend([
            (Method::GET,format!("/accounts/{account}/cfd_tunnel/{tid}/configurations"),None,200,response(json!({"config":{}}))),
            (Method::PUT,format!("/accounts/{account}/cfd_tunnel/{tid}/configurations"),Some(json!({"config":{"ingress":ingress}})),200,response(json!({}))),
            (Method::POST,format!("/zones/{zone}/dns_records"),Some(json!({"type":"CNAME","name":host,"content":format!("{tid}.cfargotunnel.com"),"proxied":true})),200,response(json!({}))),
            (Method::GET,format!("/accounts/{account}/cfd_tunnel/{tid}/token"),None,200,response(json!("connector-fixture"))),
        ]);
        steps.extend(prefix());
        steps.push((
            Method::GET,
            format!("/accounts/{account}/cfd_tunnel/{tid}/configurations"),
            None,
            200,
            response(json!({"config":{"ingress":[{"service":"http://127.0.0.1:9999"}]}})),
        ));
        let (api, queue, task) = fixture(steps).await;
        assert!(api
            .provision(token(), &account, &zone, host, 12005, environment)
            .await
            .is_ok());
        assert!(api
            .provision(token(), &account, &zone, host, 12005, environment)
            .await
            .unwrap_err()
            .contains("different routing"));
        assert!(queue.lock().unwrap().is_empty());
        task.abort();
    }
    #[tokio::test]
    async fn conflicting_dns_and_denied_tokens_never_mutate_remote_resources() {
        let account = "a".repeat(32);
        let zone = "b".repeat(32);
        let host = "phone.example.com";
        let (api,queue,task)=fixture(vec![
            (Method::GET,format!("/zones/{zone}"),None,200,response(json!({"id":zone,"account":{"id":account},"name":"example.com","status":"active"}))),
            (Method::GET,format!("/zones/{zone}/dns_records?name={host}&per_page=100"),None,200,response(json!([{"type":"A","name":host,"content":"192.0.2.1"}]))),
            (Method::GET,format!("/zones/{zone}"),None,403,json!({"error":"private upstream detail"})),
        ]).await;
        assert!(api
            .provision(token(), &account, &zone, host, 12005, "fixture")
            .await
            .unwrap_err()
            .contains("retained"));
        let err = api
            .provision(token(), &account, &zone, host, 12005, "fixture")
            .await
            .unwrap_err();
        assert!(err.contains("permissions"));
        assert!(!err.contains("private upstream"));
        assert!(queue.lock().unwrap().is_empty());
        task.abort();
    }
}
