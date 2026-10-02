//! Live run of the recommended stack (zbus + zbus_systemd) against the
//! running user manager: connect session bus, Subscribe, resolve a unit,
//! read its state, list units, wait briefly for a JobRemoved signal.
use std::time::Duration;

use futures_lite::stream::StreamExt;
use zbus::Connection;
use zbus_systemd::systemd1::{ManagerProxy, UnitProxy};

#[tokio::main]
async fn main() -> zbus::Result<()> {
    let conn = Connection::session().await?;
    let manager = ManagerProxy::new(&conn).await?;

    manager.subscribe().await?;
    println!("[ok] Subscribe()");

    let version: String = manager.version().await?;
    println!("[ok] Manager.Version = {version}");

    let unit_path: zbus::zvariant::OwnedObjectPath =
        manager.get_unit("dbus-broker.service".into()).await?;
    println!("[ok] GetUnit(dbus-broker.service) -> {unit_path}");

    let unit = UnitProxy::builder(&conn)
        .path(unit_path.clone())?
        .build()
        .await?;
    let active: String = unit.active_state().await?;
    let sub: String = unit.sub_state().await?;
    let load: String = unit.load_state().await?;
    println!("[ok] dbus-broker.service ActiveState={active} SubState={sub} LoadState={load}");

    let units = manager.list_units().await?;
    println!("[ok] ListUnits -> {} units", units.len());

    // Wait up to 10s for any JobRemoved (this desktop is busy; otherwise skip).
    let mut it = manager.inner().receive_signal("JobRemoved").await?;
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    let got = tokio::select! {
        sig = it.next() => sig,
        _ = &mut deadline => None,
    };
    match got {
        Some(msg) => {
            let (id, job, unit, result) =
                msg.body().deserialize::<(u32, zbus::zvariant::OwnedObjectPath, String, String)>()?;
            println!("[ok] observed JobRemoved within 10s: unit={unit} job={job} result={result} id={id}");
        }
        None => println!("[..] no JobRemoved within 10s (quiet bus); signal receiver was armed"),
    }

    Ok(())
}
