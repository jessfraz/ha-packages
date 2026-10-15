"""Cross-language contract through real HA auth and the compiled Rust service."""

import asyncio
import json
import os
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientTimeout
from homeassistant.config_entries import SOURCE_HASSIO, SOURCE_USER, ConfigEntryState
from homeassistant.core import Context, Event, HomeAssistant, callback
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.event import async_track_state_change_event
from homeassistant.helpers.service_info.hassio import HassioServiceInfo
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import async_fire_time_changed

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    SENSOR_KEYS,
)

OWNER_TOKEN = "test-owner-credential-at-least-32-characters"
INTEGRATION_TOKEN = "test-integration-credential-at-least-32-characters"
PREFIX = "/api/shipments/v1"


@dataclass
class RustBackend:
    """A disposable real service with credentials excluded from test output."""

    origin: str
    owner_token: str = field(repr=False)
    integration_token: str | None = field(default=None, repr=False)


async def produce_calendar_database(database: Path) -> None:
    """Use the real Rust Store/SyncWorker test producer, never duplicate SQL."""
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "test",
        "--locked",
        "--test",
        "ha_events",
        "calendar_projection_filters_and_pages_normalized_instants_without_tracking_history",
        "--",
        "--exact",
        cwd=Path(__file__).parents[1],
        env={**os.environ, "SHIPMENTS_TEST_CALENDAR_DATABASE": str(database)},
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    try:
        async with asyncio.timeout(120):
            output, _ = await process.communicate()
        assert process.returncode == 0, output.decode(errors="replace")[-4000:]
        assert await asyncio.to_thread(database.is_file)
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()


@pytest.fixture
async def rust_backend(
    hass: HomeAssistant,
    tmp_path: Path,
    unused_tcp_port: int,
    request: pytest.FixtureRequest,
    socket_enabled,
) -> AsyncIterator[RustBackend]:
    """Launch the actual binary with disposable SQLite and no provider access."""
    binary = Path(os.environ.get("SHIPMENTS_TEST_BINARY", "target/debug/shipments"))
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail("Build the backend first: cargo build --locked --bin shipments")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("SHIPMENTS_", "SUPERVISOR"))
        and key != "EASYPOST_API_KEY"
    }
    database = tmp_path / "shipments.sqlite3"
    mode = getattr(request, "param", False)
    if mode == "calendar":
        await produce_calendar_database(database)
    arguments = [
        "--bind",
        f"127.0.0.1:{unused_tcp_port}",
        "--database",
        str(database),
    ]
    app_mode = mode is True
    owner_token = None if app_mode else OWNER_TOKEN
    integration_token = INTEGRATION_TOKEN
    if app_mode:
        options = tmp_path / "options.json"
        await asyncio.to_thread(options.write_text, "{}")
        arguments.extend(("--options", str(options)))
    else:
        environment["SHIPMENTS_API_TOKEN"] = OWNER_TOKEN
        environment["SHIPMENTS_INTEGRATION_TOKEN"] = INTEGRATION_TOKEN
    origin = f"http://127.0.0.1:{unused_tcp_port}"
    process = await asyncio.create_subprocess_exec(
        str(await asyncio.to_thread(binary.resolve)),
        *arguments,
        env=environment,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(5):
            while True:
                if process.returncode is not None:
                    pytest.fail("The Rust service exited during startup")
                if owner_token is None:
                    try:
                        credentials = json.loads(
                            await asyncio.to_thread(
                                (tmp_path / "credentials.json").read_text
                            )
                        )
                    except FileNotFoundError:
                        await asyncio.sleep(0.01)
                        continue
                    owner_token = credentials["owner"]
                    integration_token = credentials["integration"]
                try:
                    async with async_get_clientsession(hass).get(
                        f"{origin}/v1/system",
                        headers={"Authorization": f"Bearer {owner_token}"},
                        timeout=ClientTimeout(total=1),
                    ) as response:
                        if response.status == 200:
                            break
                except ClientError, TimeoutError:
                    pass
                await asyncio.sleep(0.01)
        yield RustBackend(origin, owner_token, integration_token)
    finally:
        if process.returncode is None:
            process.terminate()
            try:
                async with asyncio.timeout(5):
                    await process.wait()
            except TimeoutError:
                process.kill()
                await process.wait()


async def test_ha_to_rust_contract(
    hass: HomeAssistant,
    hass_client,
    hass_admin_user,
    hass_user,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """Save pending parcels through HA, then edit and confirm without a provider."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_USER},
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"], {"next_step_id": "standalone"}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"],
        {
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    admin = await hass_client()
    user = await hass_client(hass_user_token)
    response = await user.post(
        f"{PREFIX}/shipments",
        json={
            "title": "Office supplies",
            "direction": "inbound",
            "source": {"namespace": "test", "key": "1"},
        },
        headers={"Idempotency-Key": "cross-language-create"},
    )
    assert response.status == 400
    response = await user.post(
        f"{PREFIX}/shipments",
        json={
            "title": "Office supplies",
            "direction": "inbound",
            "tracking": {"number": "TEST123", "carrier": "USPS"},
        },
        headers={"Idempotency-Key": "tracked-create-without-provider"},
    )
    assert response.status == 201
    shipment = await response.json()
    shipment_id = shipment["id"]
    assert shipment["status"] == "expected"
    assert shipment["sync"]["registration_state"] == "pending"
    assert shipment["sync"]["tracker_id"] is None
    added = await hass.services.async_call(
        DOMAIN,
        "add",
        {
            "title": "Second parcel",
            "direction": "outbound",
            "tracking_number": "TEST456",
        },
        context=Context(user_id=hass_user.id),
        blocking=True,
        return_response=True,
    )
    assert added["status"] == "expected"
    assert added["id"] != shipment_id
    response = await user.get(f"{PREFIX}/shipments/{added['id']}")
    assert (await response.json())["direction"] == "outbound"
    items = (await (await user.get(f"{PREFIX}/shipments")).json())["items"]
    assert {item["id"] for item in items} == {shipment_id, added["id"]}

    response = await user.get(f"{PREFIX}/shipments/{shipment_id}")
    assert response.status == 200
    shipment = await response.json()
    assert shipment["status"] == "expected"
    assert shipment["tracking"] == {"number": "TEST123", "carrier": "USPS"}
    assert shipment["direction"] == "inbound"
    response = await user.patch(
        f"{PREFIX}/shipments/{shipment_id}",
        json={"title": "Updated office supplies"},
        headers={"If-Match": response.headers["ETag"]},
    )
    assert response.status == 200
    shipment = await response.json()
    shipment_id = shipment["id"]
    etag = response.headers["ETag"]
    assert shipment["status"] == "expected"
    assert shipment["revision"] == 2
    # Burst writes share the integration's existing refresh cooldown.
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=11))
    await hass.async_block_till_done()
    assert hass.states.get("sensor.shipments_expected").state == "2"
    assert hass.states.get("sensor.shipments_inbound_active").state == "1"
    assert hass.states.get("sensor.shipments_outbound_active").state == "1"

    # Core authenticates the regular user; the bridge denies their confirmation.
    forbidden = await user.put(
        f"{PREFIX}/shipments/{shipment_id}/status-override",
        json={"status": "delivered", "reason": "Received in person"},
        headers={"If-Match": etag},
    )
    assert forbidden.status == 403

    # The native action uses the same Rust contract, ETag, and HA actor identity.
    action_result = await hass.services.async_call(
        DOMAIN,
        "set_override",
        {
            "shipment_id": shipment_id,
            "status": "delivered",
            "reason": "Received in person",
        },
        context=Context(user_id=hass_admin_user.id),
        blocking=True,
        return_response=True,
    )
    assert action_result == {"id": shipment_id, "revision": 3, "status": "delivered"}
    response = await admin.get(f"{PREFIX}/shipments/{shipment_id}")
    assert response.status == 200
    shipment = await response.json()
    assert shipment["status"] == "delivered"
    assert shipment["status_override"]["actor_id"] == f"ha:{hass_admin_user.id}"
    assert response.headers["ETag"] != etag
    # HA coalesces burst refreshes; advance its clock through the cooldown.
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=22))
    await hass.async_block_till_done()
    assert hass.states.get("sensor.shipments_delivered").state == "1"
    assert hass.states.get("sensor.shipments_inbound_active").state == "0"
    assert hass.states.get("sensor.shipments_outbound_active").state == "1"

    conflict = await admin.patch(
        f"{PREFIX}/shipments/{shipment_id}",
        json={"title": "Stale edit"},
        headers={"If-Match": etag},
    )
    assert conflict.status == 412
    events_response = await admin.get(f"{PREFIX}/shipments/{shipment_id}/events")
    assert events_response.status == 200
    events = (await events_response.json())["items"]
    assert [event["actor_id"] for event in events] == [
        f"ha:{hass_user.id}",
        f"ha:{hass_user.id}",
        f"ha:{hass_admin_user.id}",
    ]

    for parcel_id, status in [
        (shipment_id, "out_for_delivery"),
        (added["id"], "failure"),
    ]:
        await hass.services.async_call(
            DOMAIN,
            "set_override",
            {"shipment_id": parcel_id, "status": status, "reason": "Verified status"},
            context=Context(user_id=hass_admin_user.id),
            blocking=True,
            return_response=True,
        )
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=33))
    await hass.async_block_till_done()
    assert hass.states.get("sensor.shipments_active").state == "2"
    assert hass.states.get("sensor.shipments_inbound_active").state == "1"
    assert hass.states.get("sensor.shipments_inbound_out_for_delivery").state == "1"
    assert hass.states.get("sensor.shipments_outbound_active").state == "1"
    assert hass.states.get("sensor.shipments_exceptions").state == "1"
    assert hass.states.get("sensor.shipments_inbound_exceptions").state == "0"
    assert hass.states.get("sensor.shipments_outbound_exceptions").state == "1"

    # Three persisted transitions occur between polls and finish at the same
    # summary. Native events must deliver all three, independent of count deltas.
    transitions: list[str] = []

    @callback
    def changed(event: Event) -> None:
        state = event.data["new_state"]
        if state and state.state != "unavailable" and "event_type" in state.attributes:
            transitions.append(state.attributes["event_type"])

    unsubscribe = async_track_state_change_event(hass, ["event.shipments"], changed)
    session = async_get_clientsession(hass)
    owner_headers = {"Authorization": f"Bearer {rust_backend.owner_token}"}

    async def override(status: str) -> None:
        async with session.get(
            f"{rust_backend.origin}/v1/shipments/{shipment_id}", headers=owner_headers
        ) as current:
            assert current.status == 200
            current_etag = current.headers["ETag"]
        async with session.put(
            f"{rust_backend.origin}/v1/shipments/{shipment_id}/status-override",
            headers={**owner_headers, "If-Match": current_etag},
            json={"status": status, "reason": "Verified delivery transition"},
        ) as updated:
            assert updated.status == 200

    try:
        for status in ("delivered", "failure", "out_for_delivery"):
            await override(status)
        entry = configured["result"]
        await entry.runtime_data.async_refresh()
        await hass.async_block_till_done()
        assert transitions == ["delivered", "exception", "out_for_delivery"]
        assert hass.states.get("sensor.shipments_active").state == "2"
        attributes = hass.states.get("event.shipments").attributes
        assert attributes["shipment_id"] == shipment_id
        assert attributes["title"] == "Updated office supplies"
        assert attributes["carrier"] == "USPS"
        assert attributes["direction"] == "inbound"
        last_event_id = attributes["event_id"]
    finally:
        unsubscribe()

    assert await hass.config_entries.async_unload(entry.entry_id)
    await override("delivered")
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    attributes = hass.states.get("event.shipments").attributes
    assert attributes["event_type"] == "delivered"
    assert attributes["event_id"] > last_event_id


@pytest.mark.parametrize("rust_backend", [True], indirect=True, ids=["app"])
async def test_app_generated_pairing_and_loaded_check_in(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    """Pair an empty-options app without asking the user for any credential."""
    discovery_uuid = "019c6e27e55b73d187d84e01f1f75043"
    addon_slug = "testrepo_shipments"
    session = async_get_clientsession(hass)
    async with session.get(
        f"{rust_backend.origin}/v1/system",
        headers={"Authorization": f"Bearer {rust_backend.integration_token}"},
    ) as response:
        assert response.status == 200
        assert (await response.json())["credential_role"] == "integration"

    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_HASSIO},
        data=HassioServiceInfo(
            config={
                CONF_BACKEND_URL: rust_backend.origin,
                CONF_TOKEN: rust_backend.integration_token,
            },
            name="Shipments",
            slug=addon_slug,
            uuid=discovery_uuid,
        ),
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert not hass.config_entries.async_entries(DOMAIN)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    entry = result["result"]
    assert entry.state is ConfigEntryState.LOADED
    assert entry.unique_id == discovery_uuid
    assert entry.data[CONF_ADDON_SLUG] == addon_slug
    assert entry.data[CONF_BACKEND_URL] == rust_backend.origin
    for key in SENSOR_KEYS:
        assert hass.states.get(f"sensor.shipments_{key}").state == "0"

    async with session.get(
        f"{rust_backend.origin}/v1/installation",
        headers={"Authorization": f"Bearer {rust_backend.owner_token}"},
    ) as response:
        assert response.status == 200
        installation = await response.json()
    assert installation["integration"]["status"] == "pass"
    assert installation["authentication"]["status"] == "pass"
    assert installation["backup"]["status"] == "unavailable"
    assert installation["last_backup"] is None


@pytest.mark.parametrize("rust_backend", ["calendar"], indirect=True)
async def test_real_store_projection_reaches_native_inbound_calendar(
    hass: HomeAssistant, hass_client, hass_user_token, rust_backend: RustBackend
) -> None:
    """Read actual Rust-produced carrier ETAs through the authenticated HA API."""
    configured = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"], {"next_step_id": "standalone"}
    )
    configured = await hass.config_entries.flow.async_configure(
        configured["flow_id"],
        {
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    assert configured["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    client = await hass_client(hass_user_token)
    response = await client.get(
        "/api/calendars/calendar.shipments_inbound_deliveries",
        params={"start": "2031-02-17T00:00:00Z", "end": "2031-02-20T00:00:00Z"},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 4
    assert all(event["summary"] == "Example calendar parcel" for event in events)
    assert [
        datetime.fromisoformat(event["start"]["dateTime"]).astimezone(UTC).isoformat()
        for event in events
    ] == [
        "2031-02-17T18:00:00+00:00",
        "2031-02-17T18:00:00+00:00",
        "2031-02-18T13:00:00+00:00",
        "2031-02-19T13:00:00+00:00",
    ]
    assert len({event["uid"] for event in events}) == 4
