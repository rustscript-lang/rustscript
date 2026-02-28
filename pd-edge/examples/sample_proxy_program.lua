local vm = require("vm")

local header = vm.http.request.get_header("x-client-id")

if vm.http.rate_limit.allow(header, 3, 60) then
    vm.http.response.set_header("x-vm", "allowed")
    vm.http.response.set_body("request allowed")
else
    vm.http.response.set_header("x-vm", "rate-limited")
    vm.http.response.set_body("rate limit exceeded")
end
