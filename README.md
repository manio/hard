# hard (home automation rust-daemon)
This is my very customized home automation project which I am now rewriting into Rust.

The daemon is running on my Raspberry Pi in a specific ramdisk environment:  
https://skyboo.net/2017/04/rpi-creating-a-ram-disk-running-linux-environment-from-nfs-booted-raspbian/

Features:
- light/appliances control using Maxim/Dallas One Wire DS2413 sensor boards and DS2408 relay boards
- postgres connection for holding information about all sensors and it's relations
- PIR / alarm control

You can read more about elements of the system on my blog:
https://skyboo.net/
