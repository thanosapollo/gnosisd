;;; gnosis-test-edit.el --- Authoring shell tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis-edit)

(ert-deftest gnosis-edit-commands-fail-before-opening-buffers ()
  (dolist (buffer (list gnosis-edit-new-buffer-name gnosis-edit-buffer-name))
    (when-let* ((live (get-buffer buffer))) (kill-buffer live)))
  (should-error (gnosis-add-thema) :type 'gnosis-core-api-unavailable)
  (should-error (gnosis-edit-thema "thema")
                :type 'gnosis-core-api-unavailable)
  (should-not (get-buffer gnosis-edit-new-buffer-name))
  (should-not (get-buffer gnosis-edit-buffer-name)))

(provide 'gnosis-test-edit)
;;; gnosis-test-edit.el ends here
