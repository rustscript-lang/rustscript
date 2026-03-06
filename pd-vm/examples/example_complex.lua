local string = require("../stdlib/rss/strings.rss")
local add_one = require("vm").add_one
local io = require("io")
local re = require("re")
local json = require("json")

-- Complex Lua flavor example: loop + stdlib + host + closure.
local total = 0
for i = 0, 3, 1 do
    total = total + i
end

if not string.non_empty("lua") then
    total = 0
else
    total = add_one(total)
end

local base = 7
local add = function(value) return value + base end
base = 8
local closure_value = add(5)

local profile = { stats = { score = closure_value } }
local chained_score = profile?.stats?.score
local missing_score = profile?.missing?.value

local function keep(value)
    return value
end
local regex_ok = re.match("^lua$", "LUA", "i")
local payload = { lang = "lua", score = closure_value, chained = chained_score }
local payload_json = json.encode(payload)
local payload_decoded = json.decode(payload_json)
local json_score = payload_decoded.score
local io_ok = true
if true then
    io_ok = io.exists(".")
end

if regex_ok and io_ok and json_score == chained_score then
    print(keep(chained_score))
else
    print(0)
end
