use std::cell::Cell;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env::current_exe;
use std::io::{stdout, stderr};
use std::path::Path;
use std::rc::Rc;

use argparse::{ArgumentParser};
use signal::trap::Trap;
use nix::sys::signal::{SIGINT, SIGTERM, SIGCHLD};
use unshare::{Command, Namespace, reap_zombies};

use container::mount::{mount_tmpfs};
use container::nsutil::{set_namespace, unshare_namespace};
use container::container::Namespace::{NewNet, NewUts, NewMount};
use container::uidmap::get_max_uidmap;
use config::Config;
use config::command::{SuperviseInfo, Networking};
use config::command::SuperviseMode::{stop_on_failure};
use config::command::ChildCommand::{BridgeCommand};

use super::network;
use super::user::{common_child_command_env};
use super::build::build_container;
use file_util::create_dir;
use path_util::PathExt;
use process_util::{set_uidmap, convert_status};


pub fn run_supervise_command(config: &Config, workdir: &Path,
    sup: &SuperviseInfo, cmdname: String, mut args: Vec<String>)
    -> Result<i32, String>
{
    if sup.mode != stop_on_failure {
        panic!("Only stop-on-failure mode implemented");
    }
    {
        args.insert(0, "vagga ".to_string() + &cmdname);
        let mut ap = ArgumentParser::new();
        ap.set_description(sup.description.as_ref().map(|x| &x[..])
            .unwrap_or("Run multiple processes simultaneously"));
        // TODO(tailhook) implement --only and --exclude
        match ap.parse(args, &mut stdout(), &mut stderr()) {
            Ok(()) => {}
            Err(0) => return Ok(0),
            Err(_) => {
                return Ok(122);
            }
        }
    }

    let mut containers = BTreeSet::new();
    let mut containers_in_netns = vec!();
    let mut bridges = vec!();
    let mut containers_host_net = vec!();
    let mut forwards = vec!();
    let mut ports = vec!();
    for (name, child) in sup.children.iter() {
        let cont = child.get_container();
        if !containers.contains(cont) {
            containers.insert(cont.to_string());
            try!(build_container(config, cont));
        }
        if let &BridgeCommand(_) = child {
            bridges.push(name.to_string());
        } else {
            if let Some(ref netw) = child.network() {
                containers_in_netns.push(name.to_string());
                for (ext_port, int_port) in netw.ports.iter() {
                     forwards.push((*ext_port, netw.ip.clone(), *int_port));
                    ports.push(*ext_port);
                }
            } else {
                containers_host_net.push(name.to_string());
            }
        }
    }
    containers_in_netns.extend(bridges.into_iter()); // Bridges are just last
    if containers_in_netns.len() > 0 && !network::is_netns_set_up() {
        return Err(format!("Network namespace is not set up. You need to run \
            vagga _create_netns first"));
    }
    debug!("Containers {} with host neworking, {} in netns",
        containers_host_net.len(), containers_in_netns.len());

    let mut trap = Trap::trap(&[SIGINT, SIGTERM, SIGCHLD]);
    let mut children = HashMap::new();
    let mut error = false;
    for name in containers_host_net.iter() {
        let mut cmd = Command::new("/proc/self/exe");
        cmd.arg0("vagga_wrapper");
        cmd.keep_sigmask();
        cmd.arg(&cmdname);
        cmd.arg(&name);
        common_child_command_env(&mut cmd, Some(workdir));
        cmd.unshare(
            [Namespace::Mount, Namespace::Ipc, Namespace::Pid].iter().cloned());
        set_uidmap(&mut cmd, &get_max_uidmap().unwrap(), true);
        match cmd.spawn() {
            Ok(child) => { children.insert(child.pid(), (name, child)); }
            Err(e) => {
                if !error {
                    println!(
                        "---------- \
                        Process {} could not be run: {}. Shutting down \
                        -----------",
                        name, e);
                    error = true;
                }
            }
        }
    }
    let mut port_forward_guard;
    if containers_in_netns.len() > 0 {
        let gwdir = network::namespace_dir();
        let nsdir = gwdir.join("children");
        if !nsdir.exists() {
            try_msg!(create_dir(&nsdir, false),
                     "Failed to create dir: {err}");
        }
        try!(network::join_gateway_namespaces());
        try!(unshare_namespace(NewMount)
            .map_err(|e| format!("Failed to create mount namespace: {}", e)));
        try!(mount_tmpfs(&nsdir, "size=10m"));

        let bridge_ns = nsdir.join("bridge");
        let ip = try!(network::setup_bridge(&bridge_ns, &forwards));

        port_forward_guard = network::PortForwardGuard::new(
            &gwdir.join("netns"), ip, ports);
        try!(port_forward_guard.start_forwarding());

        for name in containers_in_netns.iter() {
            let child = sup.children.get(name).unwrap();
            let mut cmd = Command::new("/proc/self/exe");
            cmd.arg0("vagga_wrapper");
            cmd.keep_sigmask();
            cmd.arg(&cmdname);
            cmd.arg(&name);
            common_child_command_env(&mut cmd, Some(workdir));
            cmd.unshare(
                [Namespace::Mount, Namespace::Ipc, Namespace::Pid]
                .iter().cloned());

            try!(set_namespace(&bridge_ns, NewNet)
                .map_err(|e| format!("Error setting netns: {}", e)));
            if let &BridgeCommand(_) = child {
                // Already setup by set_namespace
                // But also need to mount namespace_dir into container
                cmd.env("VAGGA_NAMESPACE_DIR", &nsdir);
            } else {
                let netw = child.network().unwrap();
                let net_ns;
                let uts_ns;
                net_ns = nsdir.join("net.".to_string() + &netw.ip);
                uts_ns = nsdir.join("uts.".to_string() + &netw.ip);
                // TODO(tailhook) support multiple commands with same IP
                try!(network::setup_container(&net_ns, &uts_ns,
                    &name, &netw.ip,
                    &netw.hostname.as_ref().unwrap_or(name)));
                try!(set_namespace(&net_ns, NewNet)
                    .map_err(|e| format!("Error setting netns: {}", e)));
                try!(set_namespace(&uts_ns, NewUts)
                    .map_err(|e| format!("Error setting netns: {}", e)));
            }

            match cmd.spawn() {
                Ok(child) => { children.insert(child.pid(), (name, child)); }
                Err(e) => {
                    if !error {
                        println!(
                            "---------- \
                            Process {} could not be run: {}. Shutting down \
                            -----------",
                            name, e);
                        error = true;
                    }
                }
            }
        }

        // Need to set network namespace back to bridge, to keep namespace
        // alive. Otherwise bridge is dropped, and no connectivity between
        // containers.
        try!(set_namespace(&bridge_ns, NewNet)
            .map_err(|e| format!("Error setting netns: {}", e)));
    }

    let mut errcode = 0;
    if error {
        let mut errcode = 127;
        for &(_, ref child) in children.values() {
            child.signal(SIGTERM).ok();
        }
    } else {
        // Normal loop
        assert!(children.len() > 0);
        'signal_loop: for signal in trap.by_ref() {
            match signal {
                SIGINT => {
                    // SIGINT is usually a Ctrl+C so it's sent to whole process
                    // group, so we don't need to do anything special
                    println!("Received SIGINT signal. \
                        Waiting processes to stop..");
                    errcode = 128+SIGINT;
                    break;
                }
                SIGTERM => {
                    // SIGTERM is usually sent to a specific process so we
                    // forward it to children
                    println!("Received SIGTERM signal, propagating");
                    for &(_, ref child) in children.values() {
                        child.signal(SIGTERM).ok();
                    }
                    errcode = 128+SIGINT;
                    break;
                }
                SIGCHLD => {
                    for (pid, status) in reap_zombies() {
                        if let Some((name, _)) = children.remove(&pid) {
                            errcode = convert_status(status);
                            println!(
                                "---------- \
                                Process {}:{} {}. Shutting down \
                                -----------",
                                name, pid, status);
                            for (pid, status) in reap_zombies() {
                                children.remove(&pid);
                            }
                            break 'signal_loop;
                        }
                    }
                    if children.len() == 0 {
                        break;
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    // Stopping loop
    if children.len() > 0 {
        for signal in trap.by_ref() {
            match signal {
                SIGINT => {}
                SIGTERM => {
                    for &(_, ref child) in children.values() {
                        child.signal(SIGTERM).ok();
                    }
                }
                SIGCHLD => {
                    for (pid, status) in reap_zombies() {
                        children.remove(&pid);
                    }
                    if children.len() == 0 {
                        break;
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    Ok(errcode)
}
