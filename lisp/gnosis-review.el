;;; gnosis-review.el --- Gnosis review UI shell  -*- lexical-binding: t; -*-

;; Copyright (C) 2022-2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: extensions, learning

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Review presentation shell for generated Gnosis API bindings.  Review
;; commands fail before opening a buffer until the public contract stabilizes.

;;; Code:

(require 'gnosis-core)

(defvar gnosis-review-buffer-name "*gnosis*"
  "Buffer name reserved for Gnosis review.")

(defvar-keymap gnosis-review-mode-map
  "q" #'quit-window)

(define-derived-mode gnosis-review-mode special-mode "Gnosis-Review"
  "Major mode reserved for server-projected Gnosis review views."
  (setq-local truncate-lines nil))

;;;###autoload
(defun gnosis-review (&optional _target-count)
  "Start Gnosis review when generated bindings are available."
  (interactive)
  (gnosis-core-api-unavailable "review"))

;;;###autoload
(defun gnosis-review-topic (&optional _topic _target-count)
  "Start topic review when generated bindings are available."
  (interactive)
  (gnosis-core-api-unavailable "topic review"))

;;;###autoload
(defun gnosis-review-resume ()
  "Resume Gnosis review when generated bindings are available."
  (interactive)
  (gnosis-core-api-unavailable "review recovery"))

(provide 'gnosis-review)
;;; gnosis-review.el ends here
