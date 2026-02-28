(require (only-in "vm" add_one))

(define i 0)
(define total 0)

(while (< i 3)
  (set! total (+ total 1))
  (set! i (+ i 1)))

(if (/= total 3)
    (print 0)
    (print (add_one 5)))
