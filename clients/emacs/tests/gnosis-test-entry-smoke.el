;;; gnosis-test-entry-smoke.el --- Clean entrypoint smoke  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)
(require 'gnosis-dashboard)

(dolist (command '(gnosis-review gnosis-review-topic gnosis-review-resume))
  (unless (commandp command)
    (error "%S lacks a runtime autoload after requiring gnosis" command)))

(ert-deftest gnosis-entry-themata-add-loads-editor-lazily ()
  (let ((emacs (expand-file-name invocation-name invocation-directory))
        (library-directory (file-name-directory (locate-library "gnosis"))))
    (with-temp-buffer
      (let ((status
             (call-process
              emacs nil t nil "--batch" "-Q" "-L" library-directory "--eval"
              "(progn
                 (require 'cl-lib)
                 (require 'gnosis)
                 (require 'gnosis-dashboard)
                 (unless (autoloadp (symbol-function 'gnosis-edit-read-kind))
                   (error \"gnosis-edit-read-kind is not lazy\"))
                 (with-eval-after-load 'gnosis-edit
                   (fset 'gnosis-edit--request-context
                         (lambda () '(:authority test))))
                 (let ((gnosis-themata--authority 'test))
                   (cl-letf (((symbol-function 'gnosis-themata--call-with-authority)
                              (lambda (function) (funcall function)))
                             ((symbol-function 'completing-read)
                              (lambda (&rest _) \"basic\")))
                     (gnosis-themata-add)
                     (unless (and (featurep 'gnosis-edit)
                                  (derived-mode-p 'gnosis-edit-mode))
                       (error \"Dashboard Add did not open editor\")))))")))
        (unless (= status 0)
          (ert-fail (buffer-string)))))))

(ert-deftest gnosis-dashboard-module-loads-main-contract-standalone ()
  (let ((emacs (expand-file-name invocation-name invocation-directory))
        (library-directory (file-name-directory (locate-library "gnosis"))))
    (with-temp-buffer
      (let ((status
             (call-process
              emacs nil t nil "--batch" "-Q" "-L" library-directory "--eval"
              "(progn
                 (require 'gnosis-dashboard)
                 (unless (and (featurep 'gnosis)
                              (fboundp 'gnosis--dashboard-text))
                   (error \"Dashboard omitted its main contract\")))")))
        (unless (= status 0)
          (ert-fail (buffer-string)))))))

(ert-deftest gnosis-entry-and-dashboard-review-binding-dispatches ()
  (let ((gnosis-dashboard--projection
         (list :active_session (list :session_id "session-1")))
        called)
    (cl-letf (((symbol-function 'gnosis-review)
               (lambda (&optional _) (interactive) (push 'due called)))
              ((symbol-function 'gnosis-dashboard)
               (lambda () (interactive) (push 'dashboard called)))
              ((symbol-function 'gnosis-review-topic)
               (lambda () (interactive) (push 'topic called)))
              ((symbol-function 'gnosis-review-resume)
               (lambda () (interactive) (push 'resume called))))
      (call-interactively #'gnosis)
      (dolist (key '("r" "T" "R"))
        (call-interactively (keymap-lookup gnosis-dashboard-mode-map key)))
      (should (equal '(resume topic due dashboard) called)))))

(provide 'gnosis-test-entry-smoke)
;;; gnosis-test-entry-smoke.el ends here
