import random
from dataclasses import dataclass

from plexigraf_sdk import Batch, ConfigError, Metric, Scraper, register


# The SDK builds an instance from the component's TOML settings.
@dataclass
class Random(Scraper):
    prefix: str = "random"
    min: int = 0
    max: int = 100

    # Called by `plexigraf config check` and before start.
    def validate(self) -> None:
        if self.min >= self.max:
            raise ConfigError("min must be less than max")

    def scrape(self, now: int) -> Batch:
        attrs = {"source": "plexigraf"}
        batch = Batch()
        batch.push(Metric.gauge(f"{self.prefix}.int", unit="1").point(now, random.randint(self.min, self.max - 1), attrs))
        batch.push(Metric.gauge(f"{self.prefix}.float", unit="1").point(now, random.random(), attrs))
        return batch


register(Random)
