-- wrk script: random URLs from bench-urls.txt, prints percentiles at the end.
local urls = {}
local counter = 0

setup = function(thread)
  counter = counter + 1
  thread:set("id", counter)
end

init = function(args)
  for line in io.lines(os.getenv("BENCH_URLS") or "/var/www/html/bench-urls.txt") do
    if #line > 0 then urls[#urls + 1] = line end
  end
  math.randomseed(os.time() * 1000 + id)
end

request = function()
  return wrk.format("GET", urls[math.random(#urls)])
end

done = function(summary, latency, requests)
  local e = summary.errors
  io.write(string.format(
    "RESULT requests=%d duration_us=%d errors=%d non2xx=%d p50=%d p95=%d p99=%d\n",
    summary.requests, summary.duration,
    e.connect + e.read + e.write + e.timeout, e.status,
    latency:percentile(50), latency:percentile(95), latency:percentile(99)))
end
