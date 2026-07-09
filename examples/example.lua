local strings = require("../stdlib/rss/strings.rss")

local d = "12321312"
local e = "23232"

local ret = 1

if strings.non_empty(d) and strings.non_empty(e) then
    ret = 6
else
    ret =   0
end

print(ret)
