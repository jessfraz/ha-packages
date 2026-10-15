"""Verify native entities follow coordinated backend state and unload."""

from homeassistant.core import HomeAssistant

from custom_components.shipments.const import DOMAIN, SUMMARY_KEYS

from .conftest import SUMMARY, Backend


async def test_summary_entities_and_unload(hass: HomeAssistant, entry) -> None:
    states = hass.states.async_all("sensor")
    assert len(states) == len(SUMMARY_KEYS)
    assert {state.entity_id: state.state for state in states} == {
        f"sensor.shipments_{key}": str(value) for key, value in SUMMARY.items()
    }
    assert hass.services.has_service(DOMAIN, "archive")
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert all(
        state.state == "unavailable" for state in hass.states.async_all("sensor")
    )
    assert not hass.services.has_service(DOMAIN, "archive")
    # Shared HA session survives unloading and the same view can serve a reload.
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    assert len(hass.states.async_all("sensor")) == len(SUMMARY_KEYS)


async def test_failed_summary_marks_entities_unavailable(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    backend.responses[("GET", "/v1/summary")] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert all(
        state.state == "unavailable" for state in hass.states.async_all("sensor")
    )
    del backend.responses[("GET", "/v1/summary")]
    await entry.runtime_data.async_refresh()
    assert hass.states.get("sensor.shipments_active").state == "4"
