import tempfile
import unittest
from pathlib import Path

from analyze_openai_ws_cache import analyze


class CacheAuditTests(unittest.TestCase):
    def test_completed_calls_use_serving_provider_and_its_token_accounting(self):
        prefix = "EVENT event=AGENT_PROVIDER_STREAM_LIFECYCLE phase=stream_complete "
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "calls.log"
            path.write_text("\n".join([
                "KV_CACHE_USAGE: provider=Claude model=gpt-sol input=15000 cache_read=12000 read_pct=44",
                prefix + "provider=OpenAI connection_type=websocket/persistent-reuse saw_message_end=true input=15000 cache_read=12000 cache_write=0",
                prefix + "provider=OpenAI connection_type=websocket/persistent-fresh saw_message_end=false input=2000 cache_read=0 cache_write=0",
                prefix + "provider=OpenAI connection_type=websocket/persistent-fresh saw_message_end=true input=2000 cache_read=0 cache_write=0",
                prefix + "provider=Claude connection_type=https/sse saw_message_end=true input=1000 cache_read=6000 cache_write=3000",
                prefix + "provider=OpenRouter connection_type=https/sse saw_message_end=true input=1000 cache_read=6000 cache_write=3000",
            ]))
            report = analyze([str(path)])

        self.assertEqual(report["conn"], {"reuse": 1, "fresh": 1})
        self.assertEqual(report["cache"]["OpenAI"], [17000, 5000, 12000, 0, 2])
        self.assertEqual(report["cache"]["Claude"], [10000, 1000, 6000, 3000, 1])
        self.assertNotIn("OpenRouter", report["cache"])
        self.assertEqual(report["oa_readpct"], {"70-90%": 1, "0%": 1})
        self.assertEqual(report["oa_zero_tokens"], 2000)


if __name__ == "__main__":
    unittest.main()
