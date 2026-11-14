include "common";
# Convert OpenCode Go usage windows; every window is account-level and applies to
# all configured models. `percent` is percent USED. Any schema drift (missing or
# extra window, wrong type, out-of-range percent) fails the whole observation.
def exhausted: . == "exhausted" or . == "limited";
def window($name; $provider):
  (.usage[$provider]) as $w
  | if ($w | type) != "object" then error("missing usage window") else . end
  | if ($w.status | type) != "string" then error("missing usage status") else . end
  | ($w.resetsAt | reset) as $reset
  | (if $w.status == "ok" then
       if $reset == null then error("missing reset") else . end
       | ($w.percent | percent) as $used | 100 - $used
     elif ($w.status | exhausted) then 0
     else null end) as $remaining
  | {pool:"primary", window:$name, models:($ctx.models | keys), remaining_percent:$remaining,
     reset_at:$reset, observed_at:$ctx.now};
if (.usage | type) != "object" then error("missing usage")
elif (.usage | keys) != ["monthly", "rolling", "weekly"] then error("unexpected usage windows")
else [window("five_hour"; "rolling"), window("seven_day"; "weekly"), window("monthly"; "monthly")] | envelope end
