/* tag::catalog[]
Title:: Boundary nodes integration test

Goal:: Test if the Boundary handles raw and non-raw traffic as expected.

Runbook::
. Setup:
    . A running BN VM.
    . A subnet with 1 HTTP canister and 1 non-HTTP canister, both counters.
. Call into the non-HTTP canister, expecting the counter to increment.
. Call into the HTTP canister, expecting the counter to increment.
. Update the denylist to block the HTTP canister.
. Call into the HTTP canister again, but expecting a 451.

Success::
. The calls succeed with the expected values.
end::catalog[] */

use crate::{
    driver::{
        boundary_node::{BoundaryNode, BoundaryNodeVm},
        ic::{InternetComputer, Subnet},
        test_env::TestEnv,
        test_env_api::{
            retry_async, HasPublicApiUrl, HasTopologySnapshot, HasVm, HasWasm, IcNodeContainer,
            NnsInstallationBuilder, RetrieveIpv4Addr, SshSession, READY_WAIT_TIMEOUT,
            RETRY_BACKOFF,
        },
    },
    util::assert_create_agent,
};

use std::{convert::TryFrom, io::Read, net::SocketAddrV6, time::Duration};

use anyhow::{anyhow, bail, Context, Error};
use futures::stream::FuturesUnordered;
use ic_agent::{agent::http_transport::ReqwestHttpReplicaV2Transport, export::Principal, Agent};
use ic_base_types::PrincipalId;
use ic_interfaces_registry::RegistryValue;
use ic_protobuf::registry::routing_table::v1::RoutingTable as PbRoutingTable;
use ic_registry_keys::make_routing_table_record_key;
use ic_registry_nns_data_provider::registry::RegistryCanister;
use ic_registry_routing_table::RoutingTable;
use ic_registry_subnet_type::SubnetType;
use ic_utils::interfaces::ManagementCanister;
use serde::Deserialize;
use slog::{error, info, Logger};
use tokio::runtime::Runtime;

const BOUNDARY_NODE_NAME: &str = "boundary-node-1";
const COUNTER_CANISTER_WAT: &str = include_str!("../counter.wat");

struct PanicHandler {
    env: TestEnv,
    is_enabled: bool,
}

impl PanicHandler {
    fn new(env: TestEnv) -> Self {
        Self {
            env,
            is_enabled: true,
        }
    }

    fn disable(&mut self) {
        self.is_enabled = false;
    }
}

impl Drop for PanicHandler {
    fn drop(&mut self) {
        if !self.is_enabled {
            return;
        }

        std::thread::sleep(Duration::from_secs(60));

        let logger = self.env.logger();

        let boundary_node = self
            .env
            .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
            .unwrap()
            .get_snapshot()
            .unwrap();

        let (list_dependencies, exit_status) = exec_ssh_command(
            &boundary_node,
            "systemctl list-dependencies systemd-sysusers.service --all --reverse --no-pager",
        )
        .unwrap();

        info!(
            logger,
            "systemctl {BOUNDARY_NODE_NAME} = '{list_dependencies}'. Exit status = {}", exit_status,
        );
    }
}

pub fn exec_ssh_command(vm: &dyn SshSession, command: &str) -> Result<(String, i32), Error> {
    let mut channel = vm.block_on_ssh_session()?.channel_session()?;

    channel.exec(command)?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.wait_close()?;

    Ok((output, channel.exit_status()?))
}

fn get_install_url(env: &TestEnv) -> Result<(url::Url, PrincipalId), Error> {
    let subnet = env
        .topology_snapshot()
        .subnets()
        .next()
        .ok_or_else(|| anyhow!("missing subnet"))?;

    let node = subnet
        .nodes()
        .next()
        .ok_or_else(|| anyhow!("missing node"))?;

    Ok((node.get_public_url(), node.effective_canister_id()))
}

async fn create_canister(
    agent: &Agent,
    effective_canister_id: PrincipalId,
    canister_bytes: &[u8],
    arg: Option<Vec<u8>>,
) -> Result<Principal, String> {
    // Create a canister.
    let mgr = ManagementCanister::create(agent);
    let canister_id = mgr
        .create_canister()
        .as_provisional_create_with_amount(None)
        .with_effective_canister_id(effective_canister_id)
        .call_and_wait()
        .await
        .map_err(|err| format!("Couldn't create canister with provisional API: {}", err))?
        .0;

    let mut install_code = mgr.install_code(&canister_id, canister_bytes);
    if let Some(arg) = arg {
        install_code = install_code.with_raw_arg(arg)
    }

    install_code
        .call_and_wait()
        .await
        .map_err(|err| format!("Couldn't install canister: {}", err))?;

    Ok::<_, String>(canister_id)
}

#[derive(Copy, Clone)]
pub enum BoundaryNodeHttpsConfig {
    /// Acquire a playnet certificate (or fail if all have been acquired already)
    /// for the domain `ic{ix}.farm.dfinity.systems`
    /// where `ix` is the index of the acquired playnet.
    ///
    /// Then create an AAAA record pointing
    /// `ic{ix}.farm.dfinity.systems` to the IPv6 address of the BN.
    ///
    /// Also add CNAME records for
    /// `*.ic{ix}.farm.dfinity.systems` and
    /// `*.raw.ic{ix}.farm.dfinity.systems`
    /// pointing to `ic{ix}.farm.dfinity.systems`.
    ///
    /// If IPv4 has been enabled for the BN (`has_ipv4`),
    /// also add a corresponding A record pointing to the IPv4 address of the BN.
    ///
    /// Finally configure the BN with the playnet certificate.
    ///
    /// Note that if multiple BNs are created within the same
    /// farm-group, they will share the same certificate and
    /// domain name.
    /// Also all their IPv6 addresses will be added to the AAAA record
    /// and all their IPv4 addresses will be added to the A record.
    UseRealCertsAndDns,

    /// Don't create real certificates and DNS records,
    /// instead dangerously accept self-signed certificates and
    /// resolve domains on the client-side without quering DNS.
    AcceptInvalidCertsAndResolveClientSide,
}

pub fn mk_setup(bn_https_config: BoundaryNodeHttpsConfig) -> impl Fn(TestEnv) {
    move |env: TestEnv| {
        setup(bn_https_config, env);
    }
}

fn setup(bn_https_config: BoundaryNodeHttpsConfig, env: TestEnv) {
    let logger = env.logger();

    InternetComputer::new()
        .add_subnet(Subnet::new(SubnetType::System).add_nodes(1))
        .setup_and_start(&env)
        .expect("failed to setup IC under test");

    let nns_node = env
        .topology_snapshot()
        .root_subnet()
        .nodes()
        .next()
        .unwrap();

    NnsInstallationBuilder::new()
        .install(&nns_node, &env)
        .expect("Could not install NNS canisters");

    let bn = BoundaryNode::new(String::from(BOUNDARY_NODE_NAME))
        .allocate_vm(&env)
        .unwrap()
        .for_ic(&env, "");
    let bn = match bn_https_config {
        BoundaryNodeHttpsConfig::UseRealCertsAndDns => bn.use_real_certs_and_dns(),
        BoundaryNodeHttpsConfig::AcceptInvalidCertsAndResolveClientSide => bn,
    };
    bn.start(&env).expect("failed to setup BoundaryNode VM");

    // Await Replicas
    info!(&logger, "Checking readiness of all replica nodes...");
    for subnet in env.topology_snapshot().subnets() {
        for node in subnet.nodes() {
            node.await_status_is_healthy()
                .expect("Replica did not come up healthy.");
        }
    }

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    info!(&logger, "Polling registry");
    let registry = RegistryCanister::new(bn.nns_node_urls);
    let (latest, routes) = rt.block_on(retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
        let (bytes, latest) = registry.get_value(make_routing_table_record_key().into(), None).await
            .context("Failed to `get_value` from registry")?;
        let routes = PbRoutingTable::decode(bytes.as_slice())
            .context("Failed to decode registry routes")?;
        let routes = RoutingTable::try_from(routes)
            .context("Failed to convert registry routes")?;
        Ok((latest, routes))
    }))
    .expect("Failed to poll registry. This is not a Boundary Node error. It is a test environment issue.");
    info!(&logger, "Latest registry {latest}: {routes:?}");

    // Await Boundary Node
    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    info!(
        &logger,
        "Boundary node {BOUNDARY_NODE_NAME} has IPv6 {:?}",
        boundary_node.ipv6()
    );
    info!(
        &logger,
        "Boundary node {BOUNDARY_NODE_NAME} has IPv4 {:?}",
        boundary_node.block_on_ipv4().unwrap()
    );

    info!(&logger, "Waiting for routes file");
    let routes_path = "/var/opt/nginx/ic/ic_routes.js";
    let sleep_command = format!("while grep -q '// PLACEHOLDER' {routes_path}; do sleep 5; done");
    let (cmd_output, exit_status) = exec_ssh_command(&boundary_node, &sleep_command).unwrap();
    info!(
        logger,
        "{BOUNDARY_NODE_NAME} ran `{sleep_command}`: '{}'. Exit status = {exit_status}",
        cmd_output.trim(),
    );

    info!(&logger, "Checking BN health");
    boundary_node
        .await_status_is_healthy()
        .expect("Boundary node did not come up healthy.");
}

async fn install_canister(env: TestEnv, logger: Logger, path: &str) -> Result<Principal, Error> {
    let install_node = env
        .topology_snapshot()
        .subnets()
        .next()
        .unwrap()
        .nodes()
        .next()
        .map(|node| (node.get_public_url(), node.effective_canister_id()))
        .unwrap();

    info!(
        &logger,
        "creating replica agent {}",
        install_node.0.as_str()
    );
    let agent = assert_create_agent(install_node.0.as_str()).await;

    let canister = env.load_wasm(path);

    info!(&logger, "installing canister from path {}", path);
    let canister_id = create_canister(&agent, install_node.1, &canister, None)
        .await
        .expect("Could not create http_counter canister");

    // wait for canister to finish installing
    tokio::time::sleep(Duration::from_secs(5)).await;

    info!(&logger, "created canister {canister_id}");

    Ok::<_, Error>(canister_id)
}

/* tag::catalog[]
Title:: Boundary nodes binary canister test

Goal:: Install and query a binary canister

Runbook:
. Set up a subnet with 4 nodes and a boundary node.

Success:: The canister installs successfully and calls against it
return the expected responses

Coverage:: binary canisters behave as expected

end::catalog[] */

pub fn canister_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let mut install_node = None;
    for subnet in env.topology_snapshot().subnets() {
        for node in subnet.nodes() {
            install_node = Some((node.get_public_url(), node.effective_canister_id()));
        }
    }

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    rt.block_on(async move {
        info!(&logger, "Creating replica agent...");
        let agent = assert_create_agent(install_node.as_ref().unwrap().0.as_str()).await;

        info!(&logger, "installing canister");
        let canister_id = create_canister(
            &agent,
            install_node.unwrap().1,
            wat::parse_str(COUNTER_CANISTER_WAT).unwrap().as_slice(),
            None,
        )
        .await
        .expect("Could not create counter canister");

        info!(&logger, "created canister={canister_id}");

        // Wait for the canisters to finish installing
        // TODO: maybe this should be status calls?
        tokio::time::sleep(Duration::from_secs(5)).await;

        info!(&logger, "Creating BN agent...");
        let agent = retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            Ok(boundary_node.try_build_default_agent_async().await?)
        })
        .await
        .expect("Failed to create agent.");

        info!(&logger, "Calling read...");
        // We must retry the first request to a canister.
        // This is because a new canister might take a few seconds to show up in the BN's routing tables
        let read_result = retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            Ok(agent.query(&canister_id, "read").call().await?)
        })
        .await
        .unwrap();

        assert_eq!(read_result, [0; 4]);
    });

    panic_handler.disable();
}

/* tag::catalog[]
Title:: Boundary nodes HTTP canister test

Goal:: Install and query an HTTP canister

Runbook:
. Set up a subnet with 4 nodes and a boundary node.

Success:: The canister installs successfully and HTTP calls against it
return the expected responses

Coverage:: HTTP Canisters behave as expected

end::catalog[] */

pub fn http_canister_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let mut install_node = None;
    for subnet in env.topology_snapshot().subnets() {
        for node in subnet.nodes() {
            install_node = Some((node.get_public_url(), node.effective_canister_id()));
        }
    }
    let install_node = install_node.expect("No install node");

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    rt.block_on(async move {
        info!(&logger, "Creating replica agent...");
        let agent = assert_create_agent(install_node.0.as_str()).await;
        let kv_store_canister = env.load_wasm("rs/tests/test_canisters/kv_store/kv_store.wasm");

        info!(&logger, "installing canister");
        let canister_id = create_canister(&agent, install_node.1, &kv_store_canister, None)
            .await
            .expect("Could not create kv_store canister");

        info!(&logger, "created kv_store canister={canister_id}");

        // Wait for the canisters to finish installing
        // TODO: maybe this should be status calls?
        tokio::time::sleep(Duration::from_secs(5)).await;

        let client_builder = reqwest::ClientBuilder::new();
        let (client_builder, host, invalid_host) =
            if let Some(playnet) = boundary_node.get_playnet() {
                (
                    client_builder,
                    format!("{canister_id}.raw.{playnet}"),
                    format!("invalid-canister-id.raw.{playnet}"),
                )
            } else {
                let host = format!("{canister_id}.raw.ic0.app");
                let invalid_host = "invalid-canister-id.raw.ic0.app".to_string();
                let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0).into();
                let client_builder = client_builder
                    .danger_accept_invalid_certs(true)
                    .resolve(&host, bn_addr)
                    .resolve(&invalid_host, bn_addr);
                (client_builder, host, invalid_host)
            };
        let client = client_builder.build().unwrap();

        retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            let res = client
                .get(format!("https://{host}/foo"))
                .header("x-ic-test", "no-certificate")
                .send()
                .await?
                .text()
                .await?;

            if res != "'/foo' not found" {
                bail!(res)
            }

            Ok(())
        })
        .await
        .unwrap();

        // "x-ic-test", "no-certificate"
        // "x-ic-test", "streaming-callback"
        // "x-icx-require-certification", "1"

        retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            let res = client
                .put(format!("https://{host}/foo"))
                .body("bar")
                .send()
                .await?
                .text()
                .await?;

            if res != "'/foo' set to 'bar'" {
                bail!(res)
            }

            Ok(())
        })
        .await
        .unwrap();

        retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            let res = client
                .get(format!("https://{host}/foo"))
                .send()
                .await?
                .text()
                .await?;

            if res != "bar" {
                bail!(res)
            }

            Ok(())
        })
        .await
        .unwrap();

        retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            let res = client
                .get(format!("https://{host}/foo"))
                .header("x-ic-test", "streaming-callback")
                .send()
                .await?
                .text()
                .await?;

            if res != "bar" {
                bail!(res)
            }

            Ok(())
        })
        .await
        .unwrap();

        // Check that `canisterId` parameters go unused
        retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            let res = client
                .get(format!("https://{invalid_host}/?canisterId={canister_id}"))
                .send()
                .await?
                .text()
                .await?;

            if res != "Could not find a canister id to forward to." {
                bail!(res)
            }

            Ok(())
        })
        .await
        .unwrap();
    });

    panic_handler.disable();
}

/* tag::catalog[]
Title:: Boundary nodes valid Nginx configuration test

Goal:: Verify that nginx configuration is valid by running `nginx -T` on the boundary node.

Runbook:
. Set up a subnet with 4 nodes and a boundary node.
. SSH into the boundary node and execute `sudo nginx -t`

Success:: The output contains the string
`nginx: configuration file /etc/nginx/nginx.conf test is successful`

Coverage:: NGINX configuration is not broken

end::catalog[] */

pub fn nginx_valid_config_test(env: TestEnv) {
    let logger = env.logger();

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let (cmd_output, exit_status) = exec_ssh_command(&boundary_node, "sudo nginx -t 2>&1").unwrap();

    info!(
        logger,
        "nginx test result = '{}'. Exit status = {}",
        cmd_output.trim(),
        exit_status,
    );

    if !cmd_output.trim().contains("test is successful") {
        panic!("nginx config failed validation");
    }
}

/* tag::catalog[]
Title:: Boundary nodes denylist blocking test

Goal:: Ensure that access to a canister specified in the denylist is blocked

Runbook:
. Set up a subnet with 4 nodes and a boundary node.
. ?

Success:: ?

Coverage:: Blocking requests based on the denylist works

end::catalog[] */

pub fn denylist_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let mut install_node = None;
    for subnet in env.topology_snapshot().subnets() {
        for node in subnet.nodes() {
            install_node = Some((node.get_public_url(), node.effective_canister_id()));
        }
    }

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on(async move {
        info!(&logger, "creating replica agent");
        let agent = assert_create_agent(install_node.as_ref().unwrap().0.as_str()).await;

        let http_counter_canister = env.load_wasm("rs/tests/test_canisters/http_counter/http_counter.wasm");

        info!(&logger, "installing canister");
        let canister_id = create_canister(&agent, install_node.unwrap().1, &http_counter_canister, None)
            .await
            .expect("Could not create http_counter canister");

        // wait for canister to finish installing
        tokio::time::sleep(Duration::from_secs(5)).await;

        info!(&logger, "created canister={canister_id}");

        // Update the denylist and reload nginx
        let denylist_command = format!(r#"printf "\"~^{} .*$\" \"1\";\n" | sudo tee /var/opt/nginx/denylist/denylist.map && sudo service nginx reload"#, canister_id);
        let (cmd_output, exit_status) = exec_ssh_command(&boundary_node, &denylist_command).unwrap();
        info!(
            logger,
            "update denylist {BOUNDARY_NODE_NAME} with {denylist_command} to \n'{}'\n. Exit status = {}",
            cmd_output,
            exit_status,
        );

        // Wait a bit for the reload to complete
        tokio::time::sleep(Duration::from_secs(2)).await;

        let client_builder = reqwest::ClientBuilder::new();
        let (client_builder, host) = if let Some(playnet) = boundary_node.get_playnet() {
            (client_builder, playnet)
        } else {
            let host = "ic0.app";
            let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
            let client_builder = client_builder
                .danger_accept_invalid_certs(true)
                .resolve(&format!("{canister_id}.raw.{host}"),bn_addr.into());
            (client_builder, host.to_string())
        };
        let client = client_builder.build().unwrap();

        // Probe the blocked canister, we should get a 451
        retry_async(&logger, READY_WAIT_TIMEOUT, RETRY_BACKOFF, || async {
            let res = client
                .get(&format!("https://{canister_id}.raw.{host}/"))
                .send()
                .await?
                .status();

            if res != reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS {
                bail!(res)
            }


            Ok(())
        }).await.unwrap();
    });

    panic_handler.disable();
}

/* tag::catalog[]
Title:: Boundary nodes canister-allowlist blocking test

Goal:: Ensure that the canister-allowlist overrides the denylist

Success::
    A canister being present in the Allowlist overrides the restriction
    due to that canister being present in the denylist.

end::catalog[] */

pub fn canister_allowlist_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let mut install_node = None;
    for subnet in env.topology_snapshot().subnets() {
        for node in subnet.nodes() {
            install_node = Some((node.get_public_url(), node.effective_canister_id()));
        }
    }

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");
    rt.block_on(async move {
        info!(&logger, "creating replica agent");
        let agent = assert_create_agent(install_node.as_ref().unwrap().0.as_str()).await;

        let http_counter_canister = env.load_wasm("rs/tests/test_canisters/http_counter/http_counter.wasm");

        info!(&logger, "installing canister");
        let canister_id = create_canister(&agent, install_node.unwrap().1, &http_counter_canister, None)
            .await
            .expect("Could not create http_counter canister");

        // wait for canister to finish installing
        tokio::time::sleep(Duration::from_secs(5)).await;

        info!(&logger, "created canister={canister_id}");

        let client_builder = reqwest::ClientBuilder::new();
        let (client_builder, host) = if let Some(playnet) = boundary_node.get_playnet() {
            (client_builder, playnet)
        } else {
            let host = "ic0.app";
            let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
            let client_builder = client_builder
                .danger_accept_invalid_certs(true)
                .resolve(&format!("{canister_id}.raw.{host}"), bn_addr.into());
            (client_builder, host.to_string())
        };
        let client = client_builder.build().unwrap();

        // Check canister is available
        let res = client
            .get(format!("https://{canister_id}.raw.{host}/"))
            .send()
            .await
            .expect("Could not perform get request.")
            .status();

        assert_eq!(res, reqwest::StatusCode::OK, "expected OK, got {}", res);

        // Update denylist with canister ID
        let (cmd_output, exit_status) = exec_ssh_command(
            &boundary_node,
            &format!(
                r#"printf "\"~^{} .*$\" 1;\n" | sudo tee /var/opt/nginx/denylist/denylist.map"#,
                canister_id
            ),
        )
        .unwrap();

        info!(
            logger,
            "update denylist {BOUNDARY_NODE_NAME}: '{}'. Exit status = {}",
            cmd_output.trim(),
            exit_status
        );

        // Reload Nginx
        let (cmd_output, exit_status) = exec_ssh_command(
            &boundary_node,
            "sudo service nginx restart",
        )
        .unwrap();

        info!(
            logger,
            "reload nginx on {BOUNDARY_NODE_NAME}: '{}'. Exit status = {}",
            cmd_output.trim(),
            exit_status
        );

        tokio::time::sleep(Duration::from_secs(5)).await;

        // Check canister is restricted
        let res = client
            .get(format!("https://{canister_id}.raw.{host}/"))
            .send()
            .await
            .expect("Could not perform get request.")
            .status();

        assert_eq!(res, reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, "expected 451, got {}", res);

        // Update allowlist with canister ID
        let (cmd_output, exit_status) = exec_ssh_command(
            &boundary_node,
            &format!(r#"printf "{} 1;\n" | sudo tee /run/ic-node/allowlist_canisters.map && sudo mount -o ro,bind /run/ic-node/allowlist_canisters.map /etc/nginx/allowlist_canisters.map"#, canister_id),
        )
        .unwrap();

        info!(
            logger,
            "update allowlist {BOUNDARY_NODE_NAME}: '{}'. Exit status = {}",
            cmd_output.trim(),
            exit_status
        );

        // Reload Nginx
        let (cmd_output, exit_status) = exec_ssh_command(
            &boundary_node,
            "sudo service nginx restart",
        )
        .unwrap();

        info!(
            logger,
            "reload nginx on {BOUNDARY_NODE_NAME}: '{}'. Exit status = {}",
            cmd_output.trim(),
            exit_status
        );

        tokio::time::sleep(Duration::from_secs(5)).await;

        // Check canister is available
        let res = client
            .get(format!("https://{canister_id}.raw.{host}/"))
            .send()
            .await
            .expect("Could not perform get request.")
            .status();

        assert_eq!(res, reqwest::StatusCode::OK, "expected OK, got {}", res);
    });

    panic_handler.disable();
}

pub fn redirect_http_to_https_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve(host, bn_addr.into())
            .resolve(&format!("raw.{host}"), bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    let futs = FuturesUnordered::new();

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "redirect http to https";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client.get(format!("http://{host}/")).send().await?;

            if res.status() != reqwest::StatusCode::MOVED_PERMANENTLY {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != format!("https://{host}/") {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    let host = host_orig;
    futs.push(rt.spawn({
        let client = client;
        let name = "redirect raw http to https";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client.get(format!("http://raw.{host}/")).send().await?;

            if res.status() != reqwest::StatusCode::MOVED_PERMANENTLY {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != format!("https://raw.{host}/") {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn redirect_to_dashboard_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let client_builder = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(boundary_node.uses_snake_oil_certs())
        .redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .resolve(host, bn_addr.into())
            .resolve(&format!("raw.{host}"), bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    let futs = FuturesUnordered::new();

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "redirect to dashboard";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client.get(format!("https://{host}/")).send().await?;

            if res.status() != reqwest::StatusCode::FOUND {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != "https://dashboard.internetcomputer.org/" {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    let host = host_orig;
    futs.push(rt.spawn({
        let client = client;
        let name = "redirect raw to dashboard";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client.get(format!("https://raw.{host}/")).send().await?;

            if res.status() != reqwest::StatusCode::FOUND {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != "https://dashboard.internetcomputer.org/" {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn redirect_to_non_raw_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve("raw.{host}", bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    let futs = FuturesUnordered::new();

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "redirect status to non-raw domain";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://raw.{host}/api/v2/status"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::TEMPORARY_REDIRECT {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != format!("https://{host}/api/v2/status") {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "redirect query to non-raw domain";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .post(format!("https://raw.{host}/api/v2/canister/CID/query"))
                .body("body")
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::TEMPORARY_REDIRECT {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != format!("https://{host}/api/v2/canister/CID/query") {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "redirect call to non-raw domain";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .post(format!("https://raw.{host}/api/v2/canister/CID/call"))
                .body("body")
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::TEMPORARY_REDIRECT {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != format!("https://{host}/api/v2/canister/CID/call") {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    let host = host_orig;
    futs.push(rt.spawn({
        let client = client;
        let name = "redirect read_state to non-raw domain";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .post(format!("https://raw.{host}/api/v2/canister/CID/read_state"))
                .body("body")
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::TEMPORARY_REDIRECT {
                bail!("{name} failed: {}", res.status())
            }

            let location_hdr = res.headers().get("Location").unwrap().to_str().unwrap();
            if location_hdr != format!("https://{host}/api/v2/canister/CID/read_state") {
                bail!("{name} failed: wrong location header: {}", location_hdr)
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn sw_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    let canister_id = rt
        .block_on(install_canister(
            env.clone(),
            logger.clone(),
            "rs/tests/test_canisters/http_counter/http_counter.wasm",
        ))
        .unwrap();

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve(&format!("{canister_id}.{host}"), bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let futs = FuturesUnordered::new();

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "get index.html with sw.js include from root path";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.{host}/"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            let body_valid = body.contains("Internet Computer Loading")
                && body.contains(r#"<script defer src="/install-script.js">"#);
            if !body_valid {
                bail!("{name} failed: expected Service Worker loading page but got {body}")
            }

            let res = client
                .get(format!("https://{canister_id}.{host}/foo.js"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            let body_valid = body.contains("Internet Computer Loading")
                && body.contains(r#"<script defer src="/install-script.js">"#);
            if !body_valid {
                bail!("{name} failed: expected Service Worker loading page but got {body}")
            }

            Ok(())
        }
    }));

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "get index.html with sw.js include from non-root path";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.{host}/a/b/c"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            let body_valid = body.contains("Internet Computer Loading")
                && body.contains(r#"<script defer src="/install-script.js">"#);
            if !body_valid {
                bail!("{name} failed: expected Service Worker loading page but got {body}")
            }

            Ok(())
        }
    }));

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "get service-worker bundle";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.{host}/sw.js"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            if !res
                .headers()
                .get("Content-Type")
                .unwrap()
                .as_bytes()
                .eq(b"application/javascript")
            {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            if !body.contains("sourceMappingURL=sw.js.map") {
                bail!("{name} failed: expected sw.js but got {body}")
            }

            Ok(())
        }
    }));

    let host = host_orig;
    futs.push(rt.spawn({
        let client = client;
        let name = "get uninstall script";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.{host}/anything.js"))
                .header("Service-Worker", "script")
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            if !res
                .headers()
                .get("Content-Type")
                .unwrap()
                .as_bytes()
                .eq(b"application/javascript")
            {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            if !body.contains("unregister()") {
                bail!("{name} failed: expected uninstall script but got {body}")
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn icx_proxy_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    let canister_id = rt
        .block_on(install_canister(
            env.clone(),
            logger.clone(),
            "rs/tests/test_canisters/http_counter/http_counter.wasm",
        ))
        .unwrap();

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve(&format!("{canister_id}.{host}"), bn_addr.into())
            .resolve(&format!("{canister_id}.raw.{host}"), bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let futs = FuturesUnordered::new();

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "get sent to icx-proxy via /_/raw/";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.{host}/_/raw/"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            if !body.contains("Counter is 0") {
                bail!("{name} failed: expected icx-response but got {body}")
            }

            Ok(())
        }
    }));

    let host = host_orig;
    futs.push(rt.spawn({
        let client = client;
        let name = "get sent to icx-proxy via raw domain";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.raw.{host}/"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            if !body.contains("Counter is 0") {
                bail!("{name} failed: expected icx-response but got {body}")
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn direct_to_replica_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .expect("failed to get BN snapshot");

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve(host, bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let (install_url, effective_canister_id) =
        get_install_url(&env).expect("failed to get install url");

    let rt = Runtime::new().expect("failed to create tokio runtime");

    let futs = FuturesUnordered::new();

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let client = client.clone();
        let name = "status from random node";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{host}/api/v2/status"))
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            #[derive(Deserialize)]
            struct Status {
                replica_health_status: String,
            }

            let body = res.bytes().await?;

            let Status {
                replica_health_status,
            } = serde_cbor::from_slice::<Status>(&body)?;

            if replica_health_status != "healthy" {
                bail!("{name} failed: status check failed: {replica_health_status}")
            }

            Ok(())
        }
    }));

    let host = host_orig.clone();
    futs.push(rt.spawn({
        let logger = logger.clone();
        let client = client.clone();
        let install_url = install_url.clone();
        let name = "query random node";
        info!(&logger, "Starting subtest {}", name);

        async move {
            info!(&logger, "creating management agent");
            let agent = assert_create_agent(install_url.as_str()).await;

            info!(&logger, "creating canister");
            let cid = create_canister(
                &agent,
                effective_canister_id,
                wat::parse_str(COUNTER_CANISTER_WAT).unwrap().as_slice(),
                None,
            )
            .await
            .map_err(|err| anyhow!(format!("failed to create canister: {}", err)))?;

            // Wait for the canister to finish installing
            tokio::time::sleep(Duration::from_secs(5)).await;

            info!(&logger, "creating agent");
            let transport = ReqwestHttpReplicaV2Transport::create_with_client(
                format!("https://{host}/"),
                client,
            )?;

            let agent = Agent::builder().with_transport(transport).build()?;
            agent.fetch_root_key().await?;

            let out = agent.query(&cid, "read").call().await?;
            if !out.eq(&[0, 0, 0, 0]) {
                bail!(
                    "{name} failed: read failed with output {:?}, expected {:?}",
                    out,
                    &[0, 0, 0, 0],
                )
            }

            Ok(())
        }
    }));

    let host = host_orig;
    futs.push(rt.spawn({
        let logger = logger.clone();
        let client = client;
        let install_url = install_url;
        let name = "update random node";
        info!(&logger, "Starting subtest {}", name);

        async move {
            info!(&logger, "creating management agent");
            let agent = assert_create_agent(install_url.as_str()).await;

            info!(&logger, "creating canister");
            let cid = create_canister(
                &agent,
                effective_canister_id,
                wat::parse_str(COUNTER_CANISTER_WAT).unwrap().as_slice(),
                None,
            )
            .await
            .map_err(|err| anyhow!(format!("failed to create canister: {}", err)))?;

            // Wait for the canister to finish installing
            tokio::time::sleep(Duration::from_secs(5)).await;

            info!(&logger, "creating agent");
            let transport = ReqwestHttpReplicaV2Transport::create_with_client(
                format!("https://{host}/"),
                client,
            )?;

            let agent = Agent::builder().with_transport(transport).build()?;
            agent.fetch_root_key().await?;

            info!(&logger, "updating canister");
            agent.update(&cid, "write").call_and_wait().await?;

            info!(&logger, "querying canister");
            let out = agent.query(&cid, "read").call().await?;
            if !out.eq(&[1, 0, 0, 0]) {
                bail!(
                    "{name} failed: read failed with output {:?}, expected {:?}",
                    out,
                    &[1, 0, 0, 0],
                )
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn direct_to_replica_options_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .expect("failed to get BN snapshot");

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve(host, bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let (install_url, effective_canister_id) =
        get_install_url(&env).expect("failed to get install url");

    let rt = Runtime::new().expect("failed to create tokio runtime");

    let cid = rt
        .block_on(async {
            info!(&logger, "creating management agent");
            let agent = assert_create_agent(install_url.as_str()).await;

            info!(&logger, "creating canister");
            let cid = create_canister(
                &agent,
                effective_canister_id,
                wat::parse_str(COUNTER_CANISTER_WAT).unwrap().as_slice(),
                None,
            )
            .await
            .map_err(|err| anyhow!(format!("failed to create canister: {}", err)))?;

            // Wait for the canister to finish installing
            tokio::time::sleep(Duration::from_secs(5)).await;

            let out: Result<Principal, Error> = Ok(cid);
            out
        })
        .expect("failed to initialize test");

    let futs = FuturesUnordered::new();

    struct TestCase {
        name: String,
        path: String,
        allowed_methods: String,
    }

    let test_cases = [
        TestCase {
            name: "status OPTIONS".into(),
            path: "/api/v2/status".into(),
            allowed_methods: "HEAD, GET".into(),
        },
        TestCase {
            name: "query OPTIONS".into(),
            path: format!("/api/v2/canister/{cid}/query"),
            allowed_methods: "HEAD, POST".into(),
        },
        TestCase {
            name: "call OPTIONS".into(),
            path: format!("/api/v2/canister/{cid}/call"),
            allowed_methods: "HEAD, POST".into(),
        },
        TestCase {
            name: "read_status OPTIONS".into(),
            path: format!("/api/v2/canister/{cid}/read_state"),
            allowed_methods: "HEAD, POST".into(),
        },
    ];

    for tc in test_cases {
        let client = client.clone();
        let logger = logger.clone();

        let TestCase {
            name,
            path,
            allowed_methods,
        } = tc;

        let host = host_orig.clone();
        futs.push(rt.spawn(async move {
            info!(&logger, "Starting subtest {}", name);

            let mut url = reqwest::Url::parse(&format!("https://{host}"))?;
            url.set_path(&path);

            let req = reqwest::Request::new(reqwest::Method::OPTIONS, url);

            let res = client.execute(req).await?;

            if res.status() != reqwest::StatusCode::NO_CONTENT {
                bail!("{name} failed: {}", res.status())
            }

            for (k, v) in [
                ("Access-Control-Allow-Origin", "*"),
                ("Access-Control-Allow-Methods", &allowed_methods),
                ("Access-Control-Allow-Headers", "DNT,User-Agent,X-Requested-With,If-None-Match,If-Modified-Since,Cache-Control,Content-Type,Range,Cookie"),
                ("Access-Control-Expose-Headers", "Accept-Ranges,Content-Length,Content-Range"),
                ("Access-Control-Max-Age", "600"),
            ] {
                let hdr = res
                    .headers()
                    .get(k)
                    .ok_or_else(|| anyhow!("missing {k} header"))?.to_str()?;

                if hdr != v {
                    bail!("wrong {k} header: {hdr}, expected {v}")
                }
            }

            Ok(())
        }));
    }

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn direct_to_replica_rosetta_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .expect("failed to get BN snapshot");

    let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);

    let client = reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .resolve("rosetta.dfinity.network", bn_addr.into())
        .build()
        .expect("failed to build http client");

    let (install_url, effective_canister_id) =
        get_install_url(&env).expect("failed to get install url");

    let rt = Runtime::new().expect("failed to create tokio runtime");

    let futs = FuturesUnordered::new();

    futs.push(rt.spawn({
        let client = client.clone();
        let name = "rosetta: status from random node";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get("https://rosetta.dfinity.network/api/v2/status")
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            #[derive(Deserialize)]
            struct Status {
                replica_health_status: String,
            }

            let body = res.bytes().await?;

            let Status {
                replica_health_status,
            } = serde_cbor::from_slice::<Status>(&body)?;

            if replica_health_status != "healthy" {
                bail!("{name} failed: status check failed: {replica_health_status}")
            }

            Ok(())
        }
    }));

    futs.push(rt.spawn({
        let logger = logger.clone();
        let client = client.clone();
        let install_url = install_url.clone();
        let name = "rosetta: query random node";
        info!(&logger, "Starting subtest {}", name);

        async move {
            info!(&logger, "creating management agent");
            let agent = assert_create_agent(install_url.as_str()).await;

            info!(&logger, "creating canister");
            let cid = create_canister(
                &agent,
                effective_canister_id,
                wat::parse_str(COUNTER_CANISTER_WAT).unwrap().as_slice(),
                None,
            )
            .await
            .map_err(|err| anyhow!(format!("failed to create canister: {}", err)))?;

            // Wait for the canister to finish installing
            tokio::time::sleep(Duration::from_secs(5)).await;

            info!(&logger, "creating agent");
            let transport = ReqwestHttpReplicaV2Transport::create_with_client(
                "https://rosetta.dfinity.network/",
                client,
            )?;

            let agent = Agent::builder().with_transport(transport).build()?;
            agent.fetch_root_key().await?;

            info!(&logger, "querying canister");
            let out = agent.query(&cid, "read").call().await?;
            if !out.eq(&[0, 0, 0, 0]) {
                bail!(
                    "{name} failed: read failed with output {:?}, expected {:?}",
                    out,
                    &[0, 0, 0, 0],
                )
            }

            Ok(())
        }
    }));

    futs.push(rt.spawn({
        let logger = logger.clone();
        let client = client;
        let install_url = install_url;
        let name = "rosetta: update random node";
        info!(&logger, "Starting subtest {}", name);

        async move {
            info!(&logger, "creating management agent");
            let agent = assert_create_agent(install_url.as_str()).await;

            info!(&logger, "creating canister");
            let cid = create_canister(
                &agent,
                effective_canister_id,
                wat::parse_str(COUNTER_CANISTER_WAT).unwrap().as_slice(),
                None,
            )
            .await
            .map_err(|err| anyhow!(format!("failed to create canister: {}", err)))?;

            // Wait for the canister to finish installing
            tokio::time::sleep(Duration::from_secs(5)).await;

            info!(&logger, "creating agent");
            let transport = ReqwestHttpReplicaV2Transport::create_with_client(
                "https://rosetta.dfinity.network/",
                client,
            )?;

            let agent = Agent::builder().with_transport(transport).build()?;
            agent.fetch_root_key().await?;

            info!(&logger, "updating canister");
            agent.update(&cid, "write").call_and_wait().await?;

            info!(&logger, "querying canister");
            let out = agent.query(&cid, "read").call().await?;
            if !out.eq(&[1, 0, 0, 0]) {
                bail!(
                    "{name} failed: read failed with output {:?}, expected {:?}",
                    out,
                    &[1, 0, 0, 0],
                )
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

pub fn seo_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    let rt = tokio::runtime::Runtime::new().expect("Could not create tokio runtime.");

    let canister_id = rt
        .block_on(install_canister(
            env.clone(),
            logger.clone(),
            "rs/tests/test_canisters/http_counter/http_counter.wasm",
        ))
        .unwrap();

    let client_builder = reqwest::ClientBuilder::new().redirect(reqwest::redirect::Policy::none());
    let (client_builder, host_orig) = if let Some(playnet) = boundary_node.get_playnet() {
        (client_builder, playnet)
    } else {
        let host = "ic0.app";
        let bn_addr = SocketAddrV6::new(boundary_node.ipv6(), 443, 0, 0);
        let client_builder = client_builder
            .danger_accept_invalid_certs(true)
            .resolve(&format!("{canister_id}.{host}"), bn_addr.into());
        (client_builder, host.to_string())
    };
    let client = client_builder.build().unwrap();

    let futs = FuturesUnordered::new();

    let host = host_orig;
    futs.push(rt.spawn({
        let name = "get sent to icx-proxy if you're a bot";
        info!(&logger, "Starting subtest {}", name);

        async move {
            let res = client
                .get(format!("https://{canister_id}.{host}/"))
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
                )
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            if !body.contains("Counter is 0") {
                bail!("{name} failed: expected icx-response but got {body}")
            }

            // Test *.js to see if we end up in the nginx 404
            let res = client
                .get(format!("https://{canister_id}.{host}/foo.js"))
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
                )
                .send()
                .await?;

            if res.status() != reqwest::StatusCode::OK {
                bail!("{name} failed: {}", res.status())
            }

            let body = res.bytes().await?.to_vec();
            let body = String::from_utf8_lossy(&body);

            if !body.contains("Counter is 0") {
                bail!("{name} failed: expected icx-response but got {body}")
            }

            Ok(())
        }
    }));

    rt.block_on(async move {
        let mut cnt_err = 0;
        info!(&logger, "Waiting for subtests");

        for fut in futs {
            match fut.await {
                Ok(Err(err)) => {
                    error!(logger, "test failed: {}", err);
                    cnt_err += 1;
                }
                Err(err) => {
                    error!(logger, "test paniced: {}", err);
                    cnt_err += 1;
                }
                _ => {}
            }
        }

        match cnt_err {
            0 => Ok(()),
            _ => bail!("failed with {cnt_err} errors"),
        }
    })
    .expect("test suite failed");

    panic_handler.disable();
}

/* tag::catalog[]
Title:: Boundary nodes reboot test

Goal:: Reboot a boundary node

Runbook:
Start a boundary node and reboot it.

Success:: The boundary node reboots and continues to answer requests.

Coverage:: boundary nodes survive reboots

end::catalog[] */

pub fn reboot_test(env: TestEnv) {
    let logger = env.logger();

    let mut panic_handler = PanicHandler::new(env.clone());

    let boundary_node = env
        .get_deployed_boundary_node(BOUNDARY_NODE_NAME)
        .unwrap()
        .get_snapshot()
        .unwrap();

    info!(&logger, "Rebooting the boundary node VM.");
    boundary_node.vm().reboot();

    info!(
        &logger,
        "Waiting for the boundary node to get an IPv4 address."
    );
    info!(
        &logger,
        "Boundary node {BOUNDARY_NODE_NAME} has IPv4 {:?}",
        boundary_node.block_on_ipv4().unwrap()
    );

    info!(&logger, "Waiting for routes file");
    let routes_path = "/var/opt/nginx/ic/ic_routes.js";
    let sleep_command = format!("while grep -q '// PLACEHOLDER' {routes_path}; do sleep 5; done");
    let (cmd_output, exit_status) = exec_ssh_command(&boundary_node, &sleep_command).unwrap();
    info!(
        logger,
        "{BOUNDARY_NODE_NAME} ran `{sleep_command}`: '{}'. Exit status = {exit_status}",
        cmd_output.trim(),
    );

    info!(&logger, "Checking BN health");
    boundary_node
        .await_status_is_healthy()
        .expect("Boundary node did not come up healthy.");

    panic_handler.disable();
}
