"""Write Plexigraf plugins in Python.

Subclass ``Scraper``, ``Processor``, or ``Exporter`` (usually as a dataclass,
so the fields are the plugin's settings), then call ``register`` at module
level. The SDK decodes the component's TOML settings into the class and wires
it to the generated ``wit_world`` bindings, so plugin code never touches them.

Build with componentize-py, naming the world that matches the base class::

    componentize-py -d wit -w plexigraf:plugin/scraper componentize my_plugin -o my_plugin.wasm
"""

from __future__ import annotations

import dataclasses
import sys
import tomllib
from typing import Any, Iterable, Iterator

from componentize_py_types import Err
from wit_world.imports import encoding as _encoding
from wit_world.imports import host as _host
from wit_world.imports import metrics as raw
from wit_world.imports import types as _types

__all__ = [
    "Batch", "ConfigError", "Exporter", "FatalError", "Metric", "Outcome", "Processor",
    "Scraper", "TransientError", "attr", "decode", "encode", "log", "raw", "register",
    "secret", "state_load", "state_save",
]


# ---- Errors ----

class PluginError(Exception):
    """Base for errors that carry a kind to the host."""


class ConfigError(PluginError):
    """Bad settings; the component fails to start."""


class TransientError(PluginError):
    """A failure worth retrying."""


class FatalError(PluginError):
    """Stops the component."""


def _to_wit(e: BaseException, default: type) -> Err:
    kind = type(e) if isinstance(e, PluginError) else default
    msg = str(e) or type(e).__name__
    if kind is ConfigError:
        return Err(_types.PluginError_Config(msg))
    if kind is FatalError:
        return Err(_types.PluginError_Fatal(msg))
    return Err(_types.PluginError_Transient(msg))


# ---- Attribute values ----

def _leaf(v: Any) -> Any:
    if v is None:
        return raw.Leaf_Empty()
    if isinstance(v, bool):
        return raw.Leaf_Bool(v)
    if isinstance(v, int):
        return raw.Leaf_Int(v)
    if isinstance(v, float):
        return raw.Leaf_Double(v)
    if isinstance(v, (bytes, bytearray)):
        return raw.Leaf_Bytes(bytes(v))
    return raw.Leaf_Str(str(v))


def _any_value(v: Any) -> Any:
    """Convert a Python value, including nested lists and dicts, to AnyValue.

    WIT has no recursive types, so nested values become a flat list of nodes
    where every child comes after its parent.
    """
    if not isinstance(v, (list, tuple, dict)):
        return raw.AnyValue_Leaf(_leaf(v))
    nodes: list[Any] = []

    def add(x: Any) -> int:
        i = len(nodes)
        nodes.append(None)
        if isinstance(x, dict):
            nodes[i] = raw.Node_Kvlist([(str(k), add(val)) for k, val in x.items()])
        elif isinstance(x, (list, tuple)):
            nodes[i] = raw.Node_Array([add(val) for val in x])
        else:
            nodes[i] = raw.Node_Leaf(_leaf(x))
        return i

    add(v)
    return raw.AnyValue_Tree(nodes)


def attr(key: str, value: Any) -> raw.KeyValue:
    """Build one attribute from a scalar, list, or dict."""
    return raw.KeyValue(key, _any_value(value))


def _attrs(attrs: dict[str, Any] | None) -> list[raw.KeyValue]:
    return [attr(k, v) for k, v in (attrs or {}).items()]


# ---- Metrics and batches ----

class Metric:
    """A metric under construction: one name, unit, and type, plus points."""

    def __init__(self, name: str, unit: str, kind: str, monotonic: bool = False):
        self._name, self._unit, self._kind, self._monotonic = name, unit, kind, monotonic
        self._description = ""
        self._start = 0
        self._points: list[raw.NumberDataPoint] = []

    @classmethod
    def gauge(cls, name: str, unit: str = "1") -> "Metric":
        """A sampled value, such as a temperature or utilization."""
        return cls(name, unit, "gauge")

    @classmethod
    def counter(cls, name: str, unit: str = "1") -> "Metric":
        """A monotonic, cumulative sum, such as bytes sent."""
        return cls(name, unit, "sum", monotonic=True)

    @classmethod
    def up_down(cls, name: str, unit: str = "1") -> "Metric":
        """A cumulative sum that can go down, such as memory in use."""
        return cls(name, unit, "sum", monotonic=False)

    def description(self, d: str) -> "Metric":
        self._description = d
        return self

    def start_time(self, t: int) -> "Metric":
        """Start time for the points that follow, for sums."""
        self._start = t
        return self

    def point(self, now: int, value: int | float, attrs: dict[str, Any] | None = None) -> "Metric":
        if isinstance(value, bool):
            value = int(value)
        number = raw.Number_Int(value) if isinstance(value, int) else raw.Number_Double(float(value))
        self._points.append(raw.NumberDataPoint(
            attributes=_attrs(attrs),
            start_time_unix_nano=self._start,
            time_unix_nano=now,
            value=number,
            exemplars=[],
            flags=raw.DataPointFlags(0),
        ))
        return self

    def to_raw(self) -> raw.Metric:
        if self._kind == "gauge":
            data = raw.Data_Gauge(raw.Gauge(self._points))
        else:
            data = raw.Data_Sum(raw.Sum(self._points, raw.AggregationTemporality.CUMULATIVE, self._monotonic))
        return raw.Metric(0, 0, self._name, self._description, self._unit, data, [])


class Batch:
    """A host-owned batch of metrics."""

    def __init__(self, handle: raw.Batch | None = None):
        self._h = handle if handle is not None else raw.Batch()

    def push(self, m: Metric | raw.Metric) -> None:
        self._h.push(m.to_raw() if isinstance(m, Metric) else m)

    def extend(self, ms: Iterable[Metric | raw.Metric]) -> None:
        self._h.extend([m.to_raw() if isinstance(m, Metric) else m for m in ms])

    def __len__(self) -> int:
        return self._h.len()

    def __iter__(self) -> Iterator[raw.Metric]:
        for i in range(self._h.len()):
            yield self._h.get(i)

    def __getitem__(self, i: int) -> raw.Metric:
        return self._h.get(i)

    def __setitem__(self, i: int, m: raw.Metric) -> None:
        self._h.set(i, m)

    def retain(self, keep) -> None:
        """Keep only the metrics for which ``keep(metric)`` is true, in one pass."""
        self._h.retain([bool(keep(m)) for m in self])

    def take_all(self) -> list[raw.Metric]:
        return self._h.take_all()

    def resource(self, i: int) -> raw.Resource:
        return self._h.resource_at(i)

    def scope(self, i: int) -> raw.InstrumentationScope:
        return self._h.scope_at(i)

    @property
    def handle(self) -> raw.Batch:
        return self._h


class Outcome:
    """An exporter's result for one batch, shaped like OTLP partial success."""

    @staticmethod
    def ok() -> Any:
        return _types.ExportOutcome_Ok()

    @staticmethod
    def retry(after_ms: int | None = None) -> Any:
        return _types.ExportOutcome_Retry(after_ms)

    @staticmethod
    def reject(reason: str) -> Any:
        return _types.ExportOutcome_Reject(reason)

    @staticmethod
    def partial(rejected: int, reason: str) -> Any:
        return _types.ExportOutcome_Partial(_types.PartialFailure(rejected, reason, None))


# ---- Host services ----

class _Log:
    def _emit(self, level: Any, msg: str, fields: dict[str, Any]) -> None:
        _host.log(level, msg, [_host.Kv(k, str(v)) for k, v in fields.items()])

    def debug(self, msg: str, **fields: Any) -> None:
        self._emit(_host.Level.DEBUG, msg, fields)

    def info(self, msg: str, **fields: Any) -> None:
        self._emit(_host.Level.INFO, msg, fields)

    def warn(self, msg: str, **fields: Any) -> None:
        self._emit(_host.Level.WARN, msg, fields)

    def error(self, msg: str, **fields: Any) -> None:
        self._emit(_host.Level.ERROR, msg, fields)


log = _Log()


def secret(name: str) -> str | None:
    return _host.secret(name)


def state_load() -> bytes | None:
    return _host.state_load()


def state_save(blob: bytes) -> None:
    _host.state_save(blob)


def encode(format: str, options_toml: str, batch: Batch) -> bytes:
    """Encode with the core's encodings, such as "influx" or "json"."""
    try:
        return _encoding.encode(format, options_toml, batch.handle)
    except Err as e:
        raise ValueError(e.value) from None


def decode(format: str, options_toml: str, data: bytes) -> Batch:
    try:
        return Batch(_encoding.decode(format, options_toml, data))
    except Err as e:
        raise ValueError(e.value) from None


# ---- Plugin base classes ----

class _Plugin:
    def validate(self) -> None:
        """Check settings without side effects; `plexigraf config check` calls this."""

    def start(self) -> None:
        """Called once after settings are decoded."""

    def shutdown(self) -> None:
        """Called before the component stops."""


class Scraper(_Plugin):
    """Polls on an interval. Implement ``scrape(now) -> Batch``."""

    def scrape(self, now: int) -> Batch:
        raise NotImplementedError


class Processor(_Plugin):
    """Transforms batches. Implement ``process(batch) -> Batch``."""

    def process(self, batch: Batch) -> Batch:
        raise NotImplementedError

    def flush(self, now: int) -> Batch:
        """Called on a timer only when the manifest sets ``flush = true``."""
        return Batch()


class Exporter(_Plugin):
    """Sends batches somewhere. Implement ``export(batch) -> Outcome``."""

    def export(self, batch: Batch) -> Any:
        raise NotImplementedError


def _release(b: raw.Batch) -> None:
    """Give a batch handle back to the host."""
    b.__exit__(None, None, None)


def _build(cls: type, config_toml: str) -> Any:
    try:
        settings = tomllib.loads(config_toml)
    except tomllib.TOMLDecodeError as e:
        raise ConfigError(f"invalid TOML: {e}") from None
    if dataclasses.is_dataclass(cls):
        known = {f.name for f in dataclasses.fields(cls)}
        unknown = sorted(set(settings) - known)
        if unknown:
            raise ConfigError(f"unknown setting(s): {', '.join(unknown)}")
    try:
        inst = cls(**settings)
    except TypeError as e:
        raise ConfigError(str(e)) from None
    inst.validate()
    return inst


def register(cls: type) -> type:
    """Make ``cls`` the plugin. Call once at module level.

    componentize-py runs module import at build time and snapshots the
    result, so this costs nothing at agent startup.
    """
    state: dict[str, Any] = {}

    def plugin() -> Any:
        if "inst" not in state:
            raise Err(_types.PluginError_Fatal("plugin called before start"))
        return state["inst"]

    class Lifecycle:
        def validate_config(self, config_toml: str) -> None:
            try:
                _build(cls, config_toml)
            except Exception as e:
                raise _to_wit(e, ConfigError)

        def start(self, config_toml: str) -> None:
            try:
                inst = _build(cls, config_toml)
                inst.start()
                state["inst"] = inst
            except Exception as e:
                raise _to_wit(e, ConfigError)

        def shutdown(self) -> None:
            inst = state.pop("inst", None)
            if inst is not None:
                inst.shutdown()

    if issubclass(cls, Scraper):
        class WitWorld:
            def scrape(self, now_unix_nano: int) -> raw.Batch:
                try:
                    out = plugin().scrape(now_unix_nano)
                except Err:
                    raise
                except Exception as e:
                    raise _to_wit(e, TransientError)
                return (out or Batch()).handle
    elif issubclass(cls, Processor):
        class WitWorld:
            def process(self, input: raw.Batch) -> raw.Batch:
                try:
                    out = plugin().process(Batch(input))
                except Err:
                    _release(input)
                    raise
                except Exception as e:
                    _release(input)
                    raise _to_wit(e, TransientError)
                out = out or Batch()
                if out.handle is not input:
                    _release(input)
                return out.handle

            def flush(self, now_unix_nano: int) -> raw.Batch:
                try:
                    return (plugin().flush(now_unix_nano) or Batch()).handle
                except Err:
                    raise
                except Exception as e:
                    raise _to_wit(e, TransientError)
    elif issubclass(cls, Exporter):
        class WitWorld:
            def export(self, b: raw.Batch) -> Any:
                try:
                    return plugin().export(Batch(b))
                except Err:
                    raise
                except Exception as e:
                    raise _to_wit(e, TransientError)
                finally:
                    _release(b)
    else:
        raise TypeError("register() needs a subclass of Scraper, Processor, or Exporter")

    # componentize-py looks for the world's own exports as `WitWorld` and for
    # the exported plexigraf:plugin/lifecycle interface as `Lifecycle`, both in the
    # app module.
    app = sys.modules[cls.__module__]
    app.WitWorld = WitWorld
    app.Lifecycle = Lifecycle
    return cls
