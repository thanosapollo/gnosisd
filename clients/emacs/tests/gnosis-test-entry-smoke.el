;;; gnosis-test-entry-smoke.el --- Clean entrypoint smoke  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)

(dolist (command '(gnosis-review gnosis-review-topic gnosis-review-resume))
  (unless (commandp command)
    (error "%S lacks a runtime autoload after requiring gnosis" command)))

(ert-deftest gnosis-entry-and-dashboard-review-bindings-dispatch ()
  (let ((gnosis-dashboard--projection
         (list :active_session (list :session_id "session-1")))
        called)
    (cl-letf (((symbol-function 'gnosis-review)
               (lambda (&optional _) (interactive) (push 'due called)))
              ((symbol-function 'gnosis-dashboard)
               (lambda () (interactive) (push 'dashboard called)))
              ((symbol-function 'gnosis-dashboard-themata)
               (lambda () (interactive) (push 'themata called)))
              ((symbol-function 'gnosis-review-topic)
               (lambda () (interactive) (push 'topic called)))
              ((symbol-function 'gnosis-review-resume)
               (lambda () (interactive) (push 'resume called)))
              ((symbol-function 'gnosis-nodes-find)
               (lambda () (interactive) (push 'nodes called))))
      (call-interactively #'gnosis)
      (dolist (key '("r" "T" "R" "n" "t"))
        (call-interactively (keymap-lookup gnosis-dashboard-mode-map key)))
      (should (equal '(themata nodes resume topic due dashboard) called)))))

(provide 'gnosis-test-entry-smoke)
;;; gnosis-test-entry-smoke.el ends here
