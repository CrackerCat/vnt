use std::{io, thread};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::Ordering;

use clap::Parser;
use console::style;

use crate::handle::{CurrentDeviceInfo, DEVICE_LIST, DIRECT_ROUTE_TABLE, NAT_INFO, NatInfo, SERVER_RT};
use crate::handle::registration_handler::registration;
use crate::tun_device::create_tun;

pub mod tun_device;
pub mod nat;
pub mod error;
pub mod handle;
pub mod proto;
pub mod protocol;
#[cfg(windows)]
pub mod admin_check;

#[derive(Parser, Debug)]
#[command(author = "Lu Beilin", version, about = "一个虚拟网络工具,启动后会获取一个ip,相同token下的设备之间可以用ip直接通信")]
struct Args {
    /// 32位字符
    /// 相同token的设备之间才能通信。
    /// 建议使用uuid保证唯一性。
    /// 32-bit characters.
    /// Only devices with the same token can communicate with each other.
    /// It is recommended to use uuid to ensure uniqueness
    #[arg(short, long)]
    token: String,
}

fn log_init() {
    let home = dirs::home_dir().unwrap().join(".switch");
    if !home.exists() {
        std::fs::create_dir(&home).expect(" Failed to create '.switch' directory");
    }
    let logfile = log4rs::append::file::FileAppender::builder()
        // Pattern: https://docs.rs/log4rs/*/log4rs/encode/pattern/index.html
        .encoder(Box::new(log4rs::encode::pattern::PatternEncoder::new("{d(%+)(utc)} [{f}:{L}] {h({l})} {M}:{m}{n}\n")))
        .build(home.join("switch.log"))
        .unwrap();
    let config = log4rs::Config::builder()
        .appender(log4rs::config::Appender::builder().build("logfile", Box::new(logfile)))
        .build(
            log4rs::config::Root::builder()
                .appender("logfile")
                .build(log::LevelFilter::Info),
        )
        .unwrap();
    let _ = log4rs::init_config(config);
}

fn main() {
    log_init();
    let args = Args::parse();
    #[cfg(windows)]
    if !admin_check::is_app_elevated() {
        let args: Vec<_> = std::env::args().collect();
        println!("{}", style("正在启动管理员权限执行...").red());
        if let Some(absolute_path) = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(|p| p.to_string()))
        {
            let _ = runas::Command::new(&absolute_path).args(&args[1..]).status()
                .expect("failed to execute");
        } else {
            panic!("failed to execute")
        }
        return;
    }

    #[cfg(any(unix))]
    if sudo::RunningAs::Root != sudo::check() {
        println!("{}", style("需要使用root权限执行...").red());
        sudo::escalate_if_needed().unwrap();
    }

    println!("{}", style("启动服务...").green());

    let token = args.token;
    // let d = Local::now().timestamp().to_string();
    let mac_address = mac_address::get_mac_address().unwrap().unwrap().to_string();
    let server_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(43, 139, 56, 10)), 29876);
    // let server_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127,0,0,1)), 29876);
    let mut port = 101 as u16;
    let udp = loop {
        match UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(0), port))) {
            Ok(udp) => {
                break udp;
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::AddrInUse {
                    port += 1;
                } else {
                    log::error!("创建udp失败 {:?}",e);
                    println!("创建udp失败:{:?}", e);
                    panic!()
                }
            }
        }
    };
    //注册
    let response = registration(&udp, server_address, token, mac_address).unwrap();
    {
        let ip_list = response
            .virtual_ip_list
            .iter()
            .map(|ip| Ipv4Addr::from(*ip))
            .collect();
        let mut dev = DEVICE_LIST.lock();
        dev.0 = response.epoch;
        dev.1 = ip_list;
    }
    let virtual_ip = Ipv4Addr::from(response.virtual_ip);
    let virtual_gateway = Ipv4Addr::from(response.virtual_gateway);
    let virtual_netmask = Ipv4Addr::from(response.virtual_netmask);
    println!("virtual_gateway:{:?}", virtual_gateway);
    println!("virtual_netmask:{:?}", virtual_netmask);
    println!("当前设备ip(virtual_ip):{}", style(virtual_ip).green());
    //心跳线程
    {
        let udp = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            if let Err(e) = handle::heartbeat_handler::handle_loop(udp, server_address) {
                log::error!("心跳线程停止 {:?}",e);
                println!("心跳线程停止:{:?}", e);
            }
            std::process::exit(1);
        });
    }
    //初始化nat数据
    handle::init_nat_info(response.public_ip, response.public_port as u16);
    // tun服务
    let (tun_writer, tun_reader) =
        create_tun(virtual_ip, virtual_netmask, virtual_gateway).unwrap();
    // 打洞数据通道
    let (punch_sender, cone_receiver, req_symmetric_receiver, res_symmetric_receiver) = handle::punch_handler::bounded();
    //udp数据处理
    {
        // 低优先级的udp数据通道
        let (sender, receiver) = crossbeam::channel::bounded(100);
        let udp1 = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
            if let Err(e) = handle::udp_recv_handler::recv_loop(
                udp1,
                server_address,
                sender,
                tun_writer,
                current_device,
            ) {
                log::error!("udp数据处理线程停止 {:?}",e);
                println!("udp数据处理线程停止:{:?}", e);
            }
            std::process::exit(1);
        });
        let udp1 = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
            if let Err(e) = handle::udp_recv_handler::other_loop(udp1, receiver, current_device, punch_sender) {
                log::error!("udp数据处理线程停止 {:?}",e);
                println!("udp数据处理线程停止:{:?}", e);
            }
            std::process::exit(1);
        });
    }
    //打洞处理
    {
        let udp1 = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
            if let Err(e) = handle::punch_handler::cone_handle_loop(cone_receiver, udp1, current_device) {
                log::error!("打洞响应线程停止 {:?}",e);
                println!("打洞响应线程停止:{:?}", e);
            }
        });
        let udp1 = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
            if let Err(e) = handle::punch_handler::req_symmetric_handle_loop(req_symmetric_receiver, udp1, current_device) {
                log::error!("打洞触发线程停止 {:?}",e);
                println!("打洞触发线程停止:{:?}", e);
            }
        });
        let udp1 = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
            if let Err(e) = handle::punch_handler::res_symmetric_handle_loop(res_symmetric_receiver, udp1, current_device) {
                log::error!("打洞触发线程停止 {:?}",e);
                println!("打洞触发线程停止:{:?}", e);
            }
        });
    }
    //tun数据处理
    {
        let udp = udp.try_clone().unwrap();
        let _ = thread::spawn(move || {
            let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
            if let Err(e) = handle::tun_handler::handle_loop(udp, tun_reader, current_device) {
                log::error!("tun数据处理线程停止 {:?}",e);
                println!("tun数据处理线程停止:{:?}", e);
            }
            std::process::exit(1);
        });
    }
    use console::Term;
    let term = Term::stdout();
    let current_device = CurrentDeviceInfo::new(virtual_ip, virtual_gateway, virtual_netmask, server_address);
    loop {
        println!("{}", style("Please enter the command (Usage: list,status,exit,help):").color256(102));
        match term.read_line() {
            Ok(cmd) => {
                command(cmd.trim(), &current_device);
            }
            Err(e) => {
                println!("read_line:{:?}", e);
                std::process::exit(1);
            }
        }
    }
}

fn command(cmd: &str, current_device: &CurrentDeviceInfo) {
    match cmd {
        "list" => {
            let server_delay = SERVER_RT.load(Ordering::Relaxed);
            let device_list_lock = DEVICE_LIST.lock();
            let (_epoch, device_list) = device_list_lock.clone();
            drop(device_list_lock);
            if device_list.is_empty() {
                println!("No other devices found");
                return;
            }
            for ip in device_list {
                if let Some(route_ref) = DIRECT_ROUTE_TABLE.get(&ip) {
                    let str = if route_ref.value().delay >= 0 {
                        format!("{}(p2p delay:{}ms)", ip, route_ref.value().delay)
                    } else {
                        format!("{}(p2p)", ip)
                    };
                    drop(route_ref);
                    println!("{}", style(str).green());
                } else {
                    let str = if server_delay >= 0 {
                        format!("{}(relay delay:{}ms)", ip, server_delay * 2)
                    } else {
                        format!("{}(relay)", ip)
                    };
                    println!("{}", style(str).blue());
                }
            }
        }
        "status" => {
            let server_delay = SERVER_RT.load(Ordering::Relaxed);
            println!("Virtual ip:{}", style(current_device.virtual_ip).green());
            println!("Virtual gateway:{}", style(current_device.virtual_gateway).green());
            println!("Relay server :{}", style(current_device.connect_server).green());
            if server_delay >= 0 {
                println!("Delay of relay server :{}", style(server_delay).green());
            }
        }
        "help" | "h" => {
            println!("Options: ");
            println!("{} , Query the virtual IP of other devices", style("list").green());
            println!("{} , View current device status", style("status").green());
            println!("{} , Exit the program", style("exit").green());
        }
        "exit" => {
            std::process::exit(1);
        }
        _ => {
            println!("command {} not fount. ", style(cmd).red());
            println!("Try to enter: '{}'", style("help").green());
        }
    }
}