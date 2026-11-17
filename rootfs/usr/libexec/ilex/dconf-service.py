"""Systemd user service entry point; never a repo hook or a root helper."""
import runpy
runpy.run_path("/usr/libexec/ilex/profile.py")["dconf_service"]()
