# Public tracking links

The shipment detail view resolves carrier links independently of its sync
provider in `web/parcel-links.js`. Missing EasyPost or AfterShip public URLs
must not hide a supported carrier link. Supplied, validated provider URLs
remain available alongside it.

DoorDash uses the original validated recipient link and is deduplicated when
that URL also appears in the provider response. Links open in a separate tab
with `noopener noreferrer`. The module uses the same ingress access checks as
the other app scripts.

| Carrier family | Official tracking destination / evidence |
| --- | --- |
| USPS, UspsShip | USPS TrackConfirmAction `tLabels` route |
| UPS, UPSDAP, UPSMailInnovations | UPS `/track` `tracknum` route |
| FedEx, FedExDefault, FedexSmartPost | FedEx `/fedextrack/` `trknbr` route |
| DHL Express, DHLExpressDefault | DHL `/us-en/home/tracking.html` with `tracking-id` and `submit=1`, opening results automatically |
| Amazon MCF | [Amazon MCF documentation](https://supplychain.amazon.com/blog/how-to-track-your-mcf-orders) identifies Swiship for MCF; non-Logistics numbers use its `/track?id=` route |
| Amazon Logistics (AmazonMcf TBA numbers), Amazon Shipping | [Amazon recipient tracker](https://track.amazon.com/) `/tracking/{number}` route |
| SpeedX | [Official support form](https://speedx.io/customer-service-and-support/) builds `https://tracking.speedx.io/{number}` |
| Sway / Returnmates | [Official tracking application](https://www.returnmates.com/tracking) accepts `/tracking/{number}` and resolves its current recipient link |
| Veho | [Official homepage](https://www.shipveho.com/) form builds `https://track.shipveho.com/#/trackingid/{number}` |
| OnTrac, OntracV3, LaserShipV2 | [Official tracking form](https://www.ontrac.com/tracking/) submits the `number` query parameter |

Routes were checked against official deployed applications and representative
real shipments on 2026-10-30. Real Logistics testing established that a TBA
number can resolve on Amazon's recipient tracker while Swiship reports it
missing. Sway's public number lookup resolves its opaque recipient URL, so the
UI links to that lookup rather than constructing the opaque destination.
DHL needs `submit=1` to show details without a second click on Track.

A carrier page may still report a package missing or require authentication;
constructing a valid link does not establish that its shipment data remains
available. Unit tests use synthetic identifiers. Real tracking numbers and
recipient URLs must never enter source, fixtures, or commits.

Query values use URLSearchParams; path and fragment identifiers use
encodeURIComponent. Unsupported carrier enum values retain valid supplied
provider links. The UI does not synthesize an aggregator tracker from an
internal tracker ID or credentials, and does not assume AmazonMws is MCF.
