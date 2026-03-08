(import (prefix "../stdlib/rss/strings.rss" string:))
(require (only-in "vm" add_one))
(require (prefix-in vm. "vm"))
(require (prefix-in io. "io"))
(require (prefix-in re. "re"))
(require (prefix-in json. "json"))

; Complex Scheme flavor example: loop + stdlib + host + closure.
(declare (print value))

(define total 0)
(for (i 0 4)
  (set! total (+ total i)))

(if (not (string:non_empty "scheme"))
    (set! total 0)
    (set! total (add_one total)))

(define base 7)
(define add (lambda (value) (+ value base)))
(set! base 8)
(define closure-value (add 5))

(define profile (hash (stats (hash (score closure-value)))))
(define chained-score profile?.stats?.score)
(define missing-score profile?.missing?.value)

(define (keep value) value)
(define regex-ok (re.match "^scheme$" "SCHEME" "i"))
(define payload
  (hash
    (lang "scheme")
    (score closure-value)
    (chained chained-score)))
(define payload-json (json.encode payload))
(define payload-decoded (json.decode payload-json))
(define json-score (hash-ref payload-decoded "score"))
(define sleep-ok (vm.runtime.sleep 100))
(define io-ok true)
(if true
    (set! io-ok (io.exists "."))
    (set! io-ok io-ok))

(if (and regex-ok io-ok sleep-ok (= json-score chained-score))
    (print (keep chained-score))
    (print 0))
