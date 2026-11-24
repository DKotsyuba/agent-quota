# Validate one provider percentage without manufacturing unknown values.
def percent:
  if type == "number" and . >= 0 and . <= 100 then . else error("invalid percentage") end;

# Tokenize a provider model spelling for exact, case-insensitive membership.
def tokens: ascii_downcase | [scan("[a-z0-9]+")];

# Parse an optional RFC3339 reset, including fractional seconds and UTC offsets.
def reset:
  if . == null then null
  elif type != "string" then error("invalid reset")
  else (capture("^(?<date>[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2})(?:\\.(?<fraction>[0-9]+))?(?<zone>Z|[+-][0-9]{2}:[0-9]{2})$") // error("invalid reset")) as $t
    | ($t.date + "Z" | fromdateiso8601) as $base
    | (if $t.zone == "Z" then 0 else
        ($t.zone[1:3] | tonumber) as $h | ($t.zone[4:6] | tonumber) as $m
        | if $h > 23 or $m > 59 then error("invalid offset")
          else (($h * 3600 + $m * 60) * (if $t.zone[0:1] == "-" then -1 else 1 end)) end
       end) as $offset
    | $base - $offset + (if ($t.fraction // "") == "" then 0 else ("0." + $t.fraction | tonumber) end)
  end;

# Require at least one window and wrap it in the public collector output contract.
def envelope:
  if length == 0 then error("no applicable quota windows") else {version:1, windows:.} end;

# Bounds shared by the normalized credit contract.
def credit_count:
  if type == "number" and . >= 0 and . <= 4294967295 and . == floor then .
  else error("invalid credit count") end;
def credit_text($max):
  if type == "string" and test("\\S") and utf8bytelength <= $max
     and (test("[\\p{Cc}]") | not) then . else error("invalid credit text") end;
def credit_rows:
  if type == "array" and length <= 64 then . else error("invalid credit rows") end;

# Accept ISO timestamps and Unix seconds or milliseconds where providers use numbers.
def credit_time:
  if type == "number" then (if . > 253402300799 then . / 1000 else . end)
  else reset end
  | if . == null or (. >= 0 and . <= 253402300799) then . else error("invalid credit time") end;

def credit_scope:
  if type != "string" then "unknown"
  elif test("^model:.*\\S") then (try credit_text(128) catch "unknown")
  else ascii_downcase
  | if . == "session" or . == "five_hour" then "five_hour"
    elif . == "week" or . == "weekly" or . == "weekly_all" or . == "seven_day" then "seven_day"
    elif . == "all" then "all" else "unknown" end
  end;

def unknown_credits: {known:false, available_count:null, credits:[]};
