# Notification blueprints

Optional automations for the Home Assistant Companion app. Requires Shipments
0.6.1 or newer and a phone or tablet registered with Home Assistant.

| Blueprint | What it sends | Import |
| --- | --- | --- |
| [Delivery updates](automation/shipments/shipment_updates.yaml) | A package title, carrier, direction, and delivery status. Tap to open the package. | [Import](https://my.home-assistant.io/redirect/blueprint_import/?blueprint_url=https%3A%2F%2Fgithub.com%2Fjessfraz%2Fha-packages%2Fblob%2Fmain%2Fblueprints%2Fautomation%2Fshipments%2Fshipment_updates.yaml) |
| [Tracking problems](automation/shipments/tracking_issues.yaml) | An alert when tracking errors persist for five minutes. Tap to see the affected packages. | [Import](https://my.home-assistant.io/redirect/blueprint_import/?blueprint_url=https%3A%2F%2Fgithub.com%2Fjessfraz%2Fha-packages%2Fblob%2Fmain%2Fblueprints%2Fautomation%2Fshipments%2Ftracking_issues.yaml) |
| [Daily delivery summary](automation/shipments/delivery_summary.yaml) | Inbound packages out for delivery and packages delivered today, at a time you choose. | [Import](https://my.home-assistant.io/redirect/blueprint_import/?blueprint_url=https%3A%2F%2Fgithub.com%2Fjessfraz%2Fha-packages%2Fblob%2Fmain%2Fblueprints%2Fautomation%2Fshipments%2Fdelivery_summary.yaml) |

Import a blueprint, select **Create automation**, and choose your phone and
shipment entities. Defaults use the integration's standard entity IDs; choose
the corresponding entities if you renamed them. Nothing is enabled just by
importing a blueprint. Create a separate automation for another phone.

Delivery updates default to inbound shipments. Choose outbound or both, and
select the statuses you want. Updates for a package replace its previous
notification. Restored states and consecutive repeated event IDs do not send
alerts. Events are delivered at least once, so these notifications should not
be used as an exactly-once workflow for consequential actions.

Tracking problems uses the combined **Sync errors** sensor. It alerts on a
new zero-to-positive transition after five minutes, rather than on every poll
or every count change. Clearing errors does not send a notification. Startup
restores are ignored; a restart or automation reload resets a pending wait.
Delivery exceptions are separate from sync errors and can be selected in
**Delivery updates**.

The daily summary uses the two inbound sensors with `items` attributes. It
lists up to five titles per section and reports the remaining count, including
when a sensor's list is truncated. It stays silent when both counts are zero
or either sensor is unavailable. Out for delivery is the carrier's current
status, not every package with an estimated arrival today; delivered today
uses actual carrier timestamps within Home Assistant's local day.

Notifications contain package titles and carriers, not tracking numbers or
source records. Your Companion app's notification and lock-screen settings
control where those titles are shown.
