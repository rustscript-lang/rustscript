(import (prefix "../stdlib/rss/strings.rss" string:))
(import (prefix-in string2: "../stdlib/rss/strings.rss"))
(import (only "../stdlib/rss/strings.rss" non_empty))
(require (only-in "vm" add_one))

; Complex Scheme flavor example: loops + stdlib + host + closure + syntax coverage.
(declare (print value))

(define total 0)
(for (i 0 4)
  (set! total (+ total i)))

(when (> total 2) (set! total (+ total 1)))
(unless (= total 0) (set! total (modulo total 7)))

(cond
  ((< total 3) (set! total 3))
  ((> total 10) (set! total 10))
  (else (set! total total)))

(case total
  ((0 1) (set! total 2))
  ((3 4 5) (set! total total))
  (else (set! total total)))

(define w 0)
(while (< w 2)
  (set! w (+ w 1))
  (when (= w 1) (continue))
  (break))

(define do-sum 0)
(do ((i 0 (+ i 1)) (acc 0 (+ acc i)))
    ((>= i 3))
  (set! do-sum acc))

(define vec (vector 1 2 3))
(vector-set! vec 1 9)
(define lst (list 1 2 3))
(define lst2 (cons 0 lst))
(define first-val (car lst2))
(define rest-val (cdr lst2))
(define second-val (cadr lst2))
(define third-val (caddr lst2))
(define list-len (length lst2))
(define list-append (append lst2 (list 4 5)))
(define list-rev (reverse lst2))

(define map-fn (lambda (x) (+ x 1)))
(define list-map (map map-fn lst2))
(define list-filter (filter (lambda (x) (> x 1)) lst2))
(define (list-size xs) (length xs))
(define list-apply (apply list-size lst2))

(define text (string-append "sche" "me"))
(define text-len (string-length text))
(define text-ref (string-ref text 1))
(define text-sub (substring text 1 3))
(define num-text (number->string 42))
(define text-num (string->number "123"))

(define h (hash (a 1) (b 2)))
(hash-set! h "c" 3)
(define h-b (hash-ref h "b"))
(define v-0 (vector-ref vec 0))
(define slice-mid (slice-range vec 0 2))
(define slice-to (slice-to vec 2))
(define slice-from (slice-from vec 1))

(define quoted '(1 "two" (3 4)))
(define let-basic (let ((x 1) (y 2)) (+ x y)))
(define let-star (let* ((x 1) (y (+ x 2))) (+ x y)))
(define let-rec
  (letrec ((loop (lambda (n) (+ n 0))))
    (loop 3)))
(define named-let (let sum ((i 0) (acc 0)) (+ i acc)))

(define eqv (eqv? 1 1))
(define eq (eq? "a" "a"))
(define equalv (equal? (list 1 2) (list 1 2)))

(define bool-and (and true (> total 0)))
(define bool-or (or false (= total 0)))
(define not-val (not false))

(define pred-zero (zero? 0))
(define pred-pos (positive? 2))
(define pred-neg (negative? -2))
(define pred-even (even? 4))
(define pred-odd (odd? 5))

(define is-null (null? null))
(define is-num (number? 3))
(define is-int (integer? 3))
(define is-str (string? text))
(define is-bool (boolean? true))
(define is-vec (vector? vec))
(define is-list (list? lst2))
(define is-pair (pair? lst2))
(define is-proc (procedure? map-fn))
(define is-symbol (symbol? 'a))

(define base 7)
(define add (lambda (value) (+ value base)))
(set! base 8)
(define closure-value (add 5))

(define profile (hash (stats (hash (score closure-value)))))
(define chained-score profile?.stats?.score)
(define missing-score profile?.missing?.value)

(define vm-call (vm.add_one 5))
(define add-one-alias (add_one 6))

(define remainder-val (remainder 17 5))
(define quotient-val (quotient 9 2))
(define abs-val (abs -7))
(define min-val (min 3 2))
(define max-val (max 3 2))

(for-each (lambda (x) (+ x 1)) (list))

(if false (begin (display "hidden") (write "hidden") (newline)))

(print closure-value)
