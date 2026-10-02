//! Empirical probe mirroring the intended ussd watchdog design, on the user
//! *session* bus (zbus 5 blocking API):
//!   1. connect, call Subscribe() (required for the manager to emit signals
//!      to API-bus peers),
//!   2. poll GetUnit until the test unit exists (out-of-band `systemd-run`
//!      creates it from the shell),
//!   3. observe the out-of-band start's signals (JobNew/PropertiesChanged/
//!      JobRemoved) with timestamps, plus ActiveState before/after,
//!   4. issue StopUnit ourselves and track the returned job's JobRemoved.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::ObjectPath;

const DEST: &str = "org.freedesktop.systemd1";
const MGR_PATH: &str = "/org/freedesktop/systemd1";
const MGR_IFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT: &str = "uss-research-test.service";

fn ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn print_signal(msg: &zbus::Message) {
    let h = msg.header();
    let member = h.member().map(|s| s.to_string()).unwrap_or_default();
    let path = h
        .path()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<none>".into());
    let iface = h.interface().map(|s| s.to_string()).unwrap_or_default();

    match member.as_str() {
        "JobNew" => {
            if let Ok((id, jpath, unit)) =
                msg.body().deserialize::<(u32, ObjectPath, String)>()
            {
                println!("[{}] JobNew     unit={} job={} id={}", ms(), unit, jpath, id);
            }
        }
        "JobRemoved" => {
            if let Ok((id, jpath, unit, result)) =
                msg.body().deserialize::<(u32, ObjectPath, String, String)>()
            {
                println!(
                    "[{}] JobRemoved unit={} job={} id={} result={}",
                    ms(),
                    unit,
                    jpath,
                    id,
                    result
                );
            }
        }
        "PropertiesChanged" => {
            // sa{sv}as — print interface + removed list; the changed dict is
            // only decoded when it is our unit.
            match msg.body().deserialize::<(String, zbus::zvariant::Value, Vec<String>)>() {
                Ok((i, changed, removed)) => {
                    let s = format!("{}", changed);
                    println!(
                        "[{}] PropertiesChanged @ {} iface={} removed={:?} changed={}",
                        ms(),
                        path,
                        i,
                        removed,
                        s
                    );
                }
                Err(_) => println!("[{}] PropertiesChanged @ {}", ms(), path),
            }
        }
        _ => println!("[{}] {} {} @ {}", ms(), iface, member, path),
    }
}

fn main() -> zbus::Result<()> {
    let conn = Connection::session()?;
    println!("[{}] connected to session bus (unique: {:?})", ms(), conn.unique_name());

    let mut mgr = Proxy::new(&conn, DEST, MGR_PATH, MGR_IFACE)?;

    // 1. Subscribe: manager only emits signals to API-bus peers while at
    //    least one peer is subscribed (per-connection, refcounted).
    mgr.call_method("Subscribe", &())?;
    println!("[{}] Subscribe() ok", ms());

    // 2. Signal-reading thread (must be continuously polled).
    let iter = MessageIterator::for_match_rule(
        "type='signal',sender='org.freedesktop.systemd1'",
        &conn,
        None,
    )?;
    std::thread::spawn(move || {
        for r in iter {
            match r {
                Ok(msg) => print_signal(&msg),
                Err(e) => {
                    println!("[{}] signal stream ended: {e}", ms());
                    break;
                }
            }
        }
    });

    // 3. Wait for the unit to exist (shell creates it out-of-band).
    let mut unit: Option<ObjectPath> = None;
    let mut props: Option<Proxy> = None;
    let wait_start = SystemTime::now();
    while unit.is_none() {
        if wait_start.elapsed().map(|d| d > Duration::from_secs(30)).unwrap_or(true) {
            println!("[{}] unit never appeared", ms());
            break;
        }
        match (|| -> zbus::Result<ObjectPath<'static>> {
            let reply = mgr.call_method("GetUnit", &(UNIT,))?;
            let body = reply.body();
            let p: ObjectPath<'_> = body.deserialize()?;
            Ok(p.into_owned())
        })() {
            Ok(p) => {
                unit = Some(p.clone().into_owned());
                println!("[{}] GetUnit -> {}", ms(), unit.as_ref().unwrap());
                match Proxy::new(
                    &conn,
                    DEST,
                    unit.as_ref().unwrap(),
                    "org.freedesktop.DBus.Properties",
                ) {
                    Ok(prop) => {
                        match prop.call_method("Get", &("org.freedesktop.systemd1.Unit", "ActiveState")) {
                            Ok(reply) => match reply.body().deserialize::<zbus::zvariant::OwnedValue>()?.downcast_ref::<String>() {
                                Ok(state) => println!("[{}] ActiveState (first seen): {state}", ms()),
                                Err(e) => eprintln!("[{}] deserialize ActiveState: {e}", ms()),
                            },
                            Err(e) => eprintln!("[{}] Properties.Get: {e}", ms()),
                        }
                        props = Some(prop);
                    }
                    Err(e) => eprintln!("[{}] Proxy::new for unit: {e}", ms()),
                }
            }
            Err(e) => {
                eprintln!("[{}] GetUnit: {e}", ms());
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }

    // 4. Once the out-of-band start made the unit active, exercise the
    //    operation path ourselves: StopUnit + track the returned job.
    let deadline = SystemTime::now() + Duration::from_secs(45);
    let mut stop_issued = false;
    while SystemTime::now() < deadline {
        let act = props.as_ref().and_then(|p| {
            p.call::<_, (&str, &str), zbus::zvariant::OwnedValue>("Get", &("org.freedesktop.systemd1.Unit", "ActiveState"))
                .ok()
                .and_then(|v| v.downcast_ref::<String>().ok())
        });
        if let Some(state) = act {
            if state == "active" && !stop_issued {
                stop_issued = true;
                let reply = mgr.call_method("StopUnit", &(UNIT, "replace"));
                match reply.and_then(|r| {
                    let body = r.body();
                    let j: ObjectPath<'_> = body.deserialize()?;
                    Ok(j.into_owned())
                }) {
                    Ok(job) => println!("[{}] StopUnit() -> job {}", ms(), job),
                    Err(e) => println!("[{}] StopUnit() failed: {e}", ms()),
                }
                let d2 = SystemTime::now() + Duration::from_secs(20);
                while SystemTime::now() < d2 {
                    let s2 = props.as_ref().and_then(|p| {
                        p.call::<_, (&str, &str), zbus::zvariant::OwnedValue>("Get", &("org.freedesktop.systemd1.Unit", "ActiveState"))
                            .ok()
                            .and_then(|v| v.downcast_ref::<String>().ok())
                    });
                    if let Some(s2) = s2 {
                        if s2 != "active" {
                            println!("[{}] unit back to {s2:?} — operation path verified, DONE", ms());
                            return Ok(());
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                println!("[{}] stop did not complete in 20s", ms());
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("[{}] deadline reached (unit never observed active)", ms());
    Ok(())
}
