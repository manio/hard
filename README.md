# hard (home automation rust-daemon)
This is my very customized home automation project written in [Rust](https://www.rust-lang.org/).

The core functionality of this daemon is controlling the lights in my home, based on [PIR sensor](https://en.wikipedia.org/wiki/Passive_infrared_sensor) detection.
Those sensors are connected to one wire [DS2413](https://www.maximintegrated.com/en/products/interface/controllers-expanders/DS2413.html) sensor boards.<br>
Over a time, more features and supported devices were added to this daemon.

Features:
- light/appliances control using Maxim/Dallas One Wire DS2413 sensor boards and [DS2408 relay boards](https://skyboo.net/2017/03/controlling-relay-board-with-ds2408-over-1-wire/)
- DS1820 temperature sensor reading
- automatic night-mode based on current sun position
- [PostgreSQL](https://www.postgresql.org/) connection for holding information about all sensors and it's relations
- [InfluxDB](https://www.influxdata.com/products/influxdb/) Time Series Database support for collecting misc stats
- PIR sensors / alarm control
- [Yeelight](https://www.yeelight.com/) LED Smart Bulb on/off control
- [Rocket](https://rocket.rs/) based embedded webserver for very simple remote control
- [HIH-4000-003 humidity sensor](https://skyboo.net/2017/03/ds2438-based-1-wire-humidity-sensor/) support and automatic fan control
- doorbell support
- wicket's electric strike control
- LCD display support:
  - direct [ethlcd](http://manio.skyboo.net/ethlcd/) device connection for beeping
  - [LCDproc](http://lcdproc.omnipotent.net/) client
- USB RFID reader and tags support for specified actions
- skymax (aka [Voltronic Power](https://voltronicpower.com/)) inverter support
- remeha (aka De Dietrich) boiler support
- Huawei SUN2000 inverter support

The daemon is running on my Raspberry Pi in a specific minimal ramdisk environment:<br>
https://skyboo.net/2017/04/rpi-creating-a-ram-disk-running-linux-environment-from-nfs-booted-raspbian/

You can read more about elements of the system on my blog:<br>
https://skyboo.net/
