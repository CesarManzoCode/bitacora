//! `bitacora` command line. Subcommands arrive in later sprints.

const USAGE: &str = "\
usage: bitacora <command> [options]

commands:
  serve      serve a volume over a Unix socket
  status     print the status of a served volume
  replica    run a replica process
  elect      elect a primary among replicas
  migrate    live-migrate a volume between servers
  capacity   project physical capacity from a write history
  ublk       expose a volume as a ublk block device (feature `ublk`)";

fn main() {
    println!("{USAGE}");
}
