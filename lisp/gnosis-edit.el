;;; gnosis-edit.el --- Gnosis authoring UI shell  -*- lexical-binding: t; -*-

;; Copyright (C) 2023-2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: extensions, learning

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Text-first authoring shell for generated Gnosis API bindings.  Authoring
;; commands fail before opening a buffer until the public contract stabilizes.

;;; Code:

(require 'gnosis-core)

(defvar gnosis-edit-new-buffer-name "*Gnosis New*"
  "Buffer name reserved for adding Gnosis content.")

(defvar gnosis-edit-buffer-name "*Gnosis Edit*"
  "Buffer name reserved for editing Gnosis content.")

(defvar-keymap gnosis-edit-mode-map
  "C-c C-c" #'gnosis-edit-save
  "C-c C-k" #'gnosis-edit-cancel)

(define-derived-mode gnosis-edit-mode text-mode "Gnosis-Edit"
  "Major mode reserved for generated Gnosis authoring forms.")

;;;###autoload
(defun gnosis-add-thema (&optional _after-save)
  "Open the Gnosis authoring UI when generated bindings are available."
  (interactive)
  (gnosis-core-api-unavailable "authoring"))

;;;###autoload
(defun gnosis-edit-thema (&optional _thema-id _after-save)
  "Open the Gnosis editing UI when generated bindings are available."
  (interactive)
  (gnosis-core-api-unavailable "editing"))

(defun gnosis-edit-save ()
  "Save through generated Gnosis bindings when they are available."
  (interactive)
  (gnosis-core-api-unavailable "authoring"))

(defun gnosis-edit-cancel ()
  "Close the current Gnosis editing shell without mutation."
  (interactive)
  (unless (derived-mode-p 'gnosis-edit-mode)
    (user-error "Not in a Gnosis editing buffer"))
  (kill-buffer (current-buffer)))

(provide 'gnosis-edit)
;;; gnosis-edit.el ends here
