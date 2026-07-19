;;; gnosis-test-review.el --- Review shell tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis-review)

(ert-deftest gnosis-review-commands-fail-before-opening-review-buffer ()
  (when-let* ((buffer (get-buffer gnosis-review-buffer-name)))
    (kill-buffer buffer))
  (dolist (call '((gnosis-review)
                  (gnosis-review-topic "node")
                  (gnosis-review-resume)))
    (should-error (eval call t) :type 'gnosis-core-api-unavailable))
  (should-not (get-buffer gnosis-review-buffer-name)))

(provide 'gnosis-test-review)
;;; gnosis-test-review.el ends here
