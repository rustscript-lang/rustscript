(import (only "../stdlib/rss/strings.rss" non_empty))

(define d "12321312")
(define e "23232")

(if (and (non_empty d) (non_empty e))
    6
    0)
