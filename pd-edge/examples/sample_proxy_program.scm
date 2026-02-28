(require (prefix-in vm. "vm"))

(define header (vm.http.request.get_header "x-client-id"))

(if (vm.http.rate_limit.allow header 3 60)
    (begin
      (vm.http.response.set_header "x-vm" "allowed")
      (vm.http.response.set_body "request allowed"))
    (begin
      (vm.http.response.set_header "x-vm" "rate-limited")
      (vm.http.response.set_body "rate limit exceeded")))
