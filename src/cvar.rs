//! Console variables and the commands that inspect them.

use std::collections::BTreeMap;

use crate::cmd::Args;
use crate::info::MAX_INFO_STRING;
use crate::proxy::Proxy;
use crate::{console, cprint, dprint, info, parse};

pub const ARCHIVE: u32 = 1 << 0;
/// Mirrored into the serverinfo string.
pub const SERVERINFO: u32 = 1 << 1;
/// Settable from the command line or config during startup only.
pub const NOSET: u32 = 1 << 2;
pub const READONLY: u32 = 1 << 3;
/// Created by a `set` command rather than by the program.
pub const USER_CREATED: u32 = 1 << 4;

#[derive(Debug, Clone)]
pub struct Cvar {
    pub name: String,
    pub string: String,
    pub value: f32,
    pub integer: i32,
    pub flags: u32,
    pub modified: bool,
}

#[derive(Default)]
pub struct Cvars {
    vars: BTreeMap<String, Cvar>,
    pub serverinfo: Vec<u8>,
    /// Once startup completes, NOSET cvars become write protected.
    pub locked: bool,
}

impl Cvars {
    pub fn find(&self, name: &str) -> Option<&Cvar> {
        self.vars.get(&name.to_ascii_lowercase())
    }

    fn find_mut(&mut self, name: &str) -> Option<&mut Cvar> {
        self.vars.get_mut(&name.to_ascii_lowercase())
    }

    pub fn string(&self, name: &str) -> &str {
        self.find(name).map_or("", |v| v.string.as_str())
    }

    pub fn int(&self, name: &str) -> i32 {
        self.find(name).map_or(0, |v| v.integer)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Cvar> {
        self.vars.values()
    }

    /// Returns whether the cvar changed since the last call and clears the flag.
    pub fn take_modified(&mut self, name: &str) -> bool {
        self.find_mut(name)
            .is_some_and(|v| std::mem::take(&mut v.modified))
    }

    pub fn mark_modified(&mut self, name: &str) {
        if let Some(v) = self.find_mut(name) {
            v.modified = true;
        }
    }

    /// Registers a cvar with a default, keeping any value a config already
    /// gave it unless the cvar is read only.
    pub fn get(&mut self, name: &str, default: &str, flags: u32) {
        match self.find_mut(name) {
            Some(var) if flags & READONLY != 0 => {
                var.flags = flags;
                self.force_set(name, default);
            }
            Some(var) => var.flags |= flags,
            None => self.create(name, default, flags),
        }
        if flags & USER_CREATED == 0
            && let Some(var) = self.find_mut(name)
        {
            var.flags &= !USER_CREATED;
        }
    }

    pub fn set(&mut self, name: &str, value: &str) {
        self.set_internal(name, value, false);
    }

    pub fn force_set(&mut self, name: &str, value: &str) {
        self.set_internal(name, value, true);
    }

    /// Creates or overwrites a cvar, replacing its flags outright.
    pub fn full_set(&mut self, name: &str, value: &str, flags: u32) {
        match self.find_mut(name) {
            Some(var) => {
                var.flags = flags;
                self.force_set(name, value);
            }
            None => self.get(name, value, flags),
        }
    }

    pub fn set_value(&mut self, name: &str, value: f32) {
        self.set(name, &value.to_string());
    }

    pub fn create(&mut self, name: &str, value: &str, flags: u32) {
        if self.find(name).is_some() {
            dprint!("Cvar_Create: cvar {name} already exist, unexpected\n");
            return;
        }
        self.vars.insert(
            name.to_ascii_lowercase(),
            Cvar {
                name: name.to_owned(),
                string: String::new(),
                value: 0.0,
                integer: 0,
                flags,
                modified: false,
            },
        );
        self.force_set(name, value);
    }

    fn set_internal(&mut self, name: &str, value: &str, force: bool) {
        let locked = self.locked;
        let Some(var) = self.find_mut(name) else {
            self.create(name, value, 0);
            return;
        };

        if !force && (var.flags & READONLY != 0 || (var.flags & NOSET != 0 && locked)) {
            cprint!("{name} is write protected.\n");
            return;
        }

        var.string = value.to_owned();
        var.value = parse::atof(value.as_bytes()) as f32;
        var.integer = parse::atoi(value.as_bytes());
        var.modified = true;
        let (display_name, flags, integer) = (var.name.clone(), var.flags, var.integer);

        if display_name.eq_ignore_ascii_case("developer") {
            console::set_developer(integer);
        }

        if flags & SERVERINFO != 0
            && info::value_for_key(&self.serverinfo, display_name.as_bytes()) != value.as_bytes()
        {
            info::set_value_for_star_key(
                &mut self.serverinfo,
                display_name.as_bytes(),
                value.as_bytes(),
                MAX_INFO_STRING,
                true,
            );
        }
    }
}

impl Proxy {
    pub fn register_cvar_commands(&mut self) {
        self.cmds.register("cvarlist", cmd_cvarlist);
        self.cmds.register("toggle", cmd_toggle);
        self.cmds.register("set", cmd_set);
        self.cmds.register("inc", cmd_inc);
    }

    /// Handles `<cvar>` (print) and `<cvar> <value>` (assign) console lines.
    pub fn cvar_command(&mut self, args: &Args) -> bool {
        let Some(var) = self.cvars.find(&args.arg_str(0)) else {
            return false;
        };
        if args.argc() == 1 {
            cprint!("\"{}\" is \"{}\"\n", var.name, var.string);
        } else {
            let name = var.name.clone();
            let value = args.join(1, args.argc() - 1);
            self.cvars.set(&name, &value);
        }
        true
    }
}

fn cmd_cvarlist(proxy: &mut Proxy, _args: &Args) {
    let mut count = 0;
    for var in proxy.cvars.iter() {
        cprint!(
            "{}{} {}\n",
            if var.flags & ARCHIVE != 0 { '*' } else { ' ' },
            if var.flags & SERVERINFO != 0 {
                's'
            } else {
                ' '
            },
            var.name
        );
        count += 1;
    }
    cprint!("------------\n{count} variables\n");
}

fn cmd_toggle(proxy: &mut Proxy, args: &Args) {
    if args.argc() != 2 {
        cprint!("toggle <cvar> : toggle a cvar on/off\n");
        return;
    }
    let name = args.arg_str(1);
    let Some(var) = proxy.cvars.find(&name) else {
        cprint!("Unknown variable \"{name}\"\n");
        return;
    };
    let (name, toggled) = (var.name.clone(), if var.value != 0.0 { "0" } else { "1" });
    proxy.cvars.set(&name, toggled);
}

fn cmd_set(proxy: &mut Proxy, args: &Args) {
    if args.argc() < 3 {
        cprint!("usage: set <cvar> <value>\n");
        return;
    }
    let name = args.arg_str(1).into_owned();
    let value = args.join(2, args.argc() - 1);
    if proxy.cvars.find(&name).is_some() {
        proxy.cvars.set(&name, &value);
    } else {
        proxy.cvars.create(&name, &value, USER_CREATED);
    }
}

fn cmd_inc(proxy: &mut Proxy, args: &Args) {
    if !matches!(args.argc(), 2 | 3) {
        cprint!("inc <cvar> [value]\n");
        return;
    }
    let name = args.arg_str(1);
    let Some(var) = proxy.cvars.find(&name) else {
        cprint!("Unknown variable \"{name}\"\n");
        return;
    };
    let delta = if args.argc() == 3 {
        parse::atof(args.arg(2)) as f32
    } else {
        1.0
    };
    let (name, value) = (var.name.clone(), var.value + delta);
    proxy.cvars.set_value(&name, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_keeps_config_values_and_clears_user_flag() {
        let mut cvars = Cvars::default();
        cvars.create("net_port", "1234", USER_CREATED);
        cvars.get("net_port", "30000", NOSET);
        let var = cvars.find("NET_PORT").unwrap();
        assert_eq!(var.string, "1234");
        assert_eq!(var.integer, 1234);
        assert_eq!(var.flags, NOSET);
        assert!(var.modified);
    }

    #[test]
    fn readonly_and_noset_are_protected() {
        let mut cvars = Cvars::default();
        cvars.get("*version", "v1", READONLY);
        cvars.set("*version", "hacked");
        assert_eq!(cvars.string("*version"), "v1");

        cvars.get("net_ip", "0.0.0.0", NOSET);
        cvars.set("net_ip", "10.0.0.1");
        assert_eq!(cvars.string("net_ip"), "10.0.0.1");
        cvars.locked = true;
        cvars.set("net_ip", "10.0.0.2");
        assert_eq!(cvars.string("net_ip"), "10.0.0.1");
        cvars.full_set("net_ip", "10.0.0.3", NOSET);
        assert_eq!(cvars.string("net_ip"), "10.0.0.3");
    }

    #[test]
    fn serverinfo_mirrors_flagged_cvars() {
        let mut cvars = Cvars::default();
        cvars.get("hostname", "unnamed", SERVERINFO);
        cvars.get("developer", "0", 0);
        assert_eq!(cvars.serverinfo, b"\\hostname\\unnamed");
        cvars.set("hostname", "my proxy");
        assert_eq!(cvars.serverinfo, b"\\hostname\\my proxy");
    }

    #[test]
    fn take_modified_clears_flag() {
        let mut cvars = Cvars::default();
        cvars.get("masters", "a b", 0);
        assert!(cvars.take_modified("masters"));
        assert!(!cvars.take_modified("masters"));
        cvars.mark_modified("masters");
        assert!(cvars.take_modified("masters"));
    }
}
