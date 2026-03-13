(require (prefix-in http. "http"))
(require (prefix-in rate_limit. "rate_limit"))

;; Rate-limit callers by x-client-id: allow 3 requests per 60 seconds and expose the decision in x-vm.
(define header (http.request.get_header "x-client-id"))

(if (rate_limit.allow header 3 60)
    (begin
      (http.response.set_header "x-vm" "allowed")
      (http.response.set_body "request allowed"))
    (begin
      (http.response.set_header "x-vm" "rate-limited")
      (http.response.set_body "rate limit exceeded")))
