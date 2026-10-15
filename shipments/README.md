# Shipments

A local shipment registry and dashboard for Home Assistant. Install this app
from the App Store to run the service. Download the **Shipments integration**
through HACS to add sensors and native actions, restart Home Assistant, then
confirm the discovered integration in **Settings → Devices & services**.

Both stores use `https://github.com/jessfraz/ha-packages`. The app manages its
credentials automatically. Each shipment needs a verified tracking number.
Use EasyPost for most carriers, AfterShip for SpeedX and Sway / Returnmates,
and the original recipient tracking link for DoorDash. DoorDash needs no API
key; configure the other provider keys in the app settings.

See the Documentation tab for configuration and the repository README for
API and development instructions.
