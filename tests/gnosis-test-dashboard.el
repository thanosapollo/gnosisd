;;; gnosis-test-dashboard.el --- Foundation dashboard tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)

(ert-deftest gnosis-dashboard-shows-health-and-openapi-version ()
  (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
    (kill-buffer buffer))
  (unwind-protect
      (cl-letf (((symbol-function 'gnosis-core-status)
                 (lambda () '(:status "ready")))
                ((symbol-function 'gnosis-core-openapi)
                 (lambda () '(:openapi "3.1.0"
                              :info (:title "Gnosis API" :version "1.0.0")
                              :paths (:/v1/test (:get nil))))))
        (save-window-excursion (gnosis))
        (with-current-buffer gnosis-dashboard-buffer-name
          (should (derived-mode-p 'gnosis-dashboard-mode))
          (should (string-match-p "Daemon: ready" (buffer-string)))
          (should (string-match-p "OpenAPI: 3.1.0" (buffer-string)))
          (should (string-match-p "Paths: 1" (buffer-string)))
          (should (string-match-p "Domain commands are not yet available"
                                  (buffer-string)))))
    (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
      (kill-buffer buffer))))

(provide 'gnosis-test-dashboard)
;;; gnosis-test-dashboard.el ends here
