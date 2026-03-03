(import (only "../stdlib/rss/strings.rss" non_empty))
(require (only-in "vm" add_one))

(define d "12321312")
(define e "23232")

(if (and (non_empty d) (non_empty e))
    (add_one 5)
    0)
