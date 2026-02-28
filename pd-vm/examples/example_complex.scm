(import (prefix "../stdlib/rss/strings.rss" string:))
(require (only-in "vm" add_one))

; Complex Scheme flavor example: loop + stdlib + host + closure.
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

(print closure-value)
