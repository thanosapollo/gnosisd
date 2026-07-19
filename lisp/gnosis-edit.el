;;; gnosis-edit.el --- Gnosis thema editor  -*- lexical-binding: t; -*-

;; Copyright (C) 2023-2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: extensions, learning

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Shared text-first editor for server-validated thema request documents.

;;; Code:

(require 'gnosis-core)

(defvar gnosis-edit-new-buffer-name "*Gnosis New*"
  "Buffer name reserved for adding Gnosis content.")

(defvar gnosis-edit-buffer-name "*Gnosis Edit*"
  "Buffer name used for editing a Gnosis thema.")

(defvar-local gnosis-edit--thema-id nil
  "Opaque thema identity for the current edit buffer.")

(defvar-local gnosis-edit--revision nil
  "Authoritative thema revision for the current edit buffer.")

(defvar-local gnosis-edit--after-save nil
  "Function called with the validated server projection after save.")

(defvar-local gnosis-edit--write-uncertain nil
  "Non-nil after a mutation whose server outcome cannot be determined.")

(defvar-keymap gnosis-edit-mode-map
  "C-c C-c" #'gnosis-edit-save
  "C-c C-k" #'gnosis-edit-cancel)

(define-derived-mode gnosis-edit-mode text-mode "Gnosis-Edit"
  "Major mode for editing one Gnosis thema request as JSON."
  (setq-local header-line-format
              (substitute-command-keys
               " Save with \\[gnosis-edit-save], cancel with \\[gnosis-edit-cancel]")))

(defun gnosis-edit--new-request ()
  "Return a complete initial thema request document."
  `(:kind "basic" :keimenon "" :hypothesis [] :answer []
    :source_guid ,gnosis-core--json-null
    :parathema ,gnosis-core--json-null
    :review_image ,gnosis-core--json-null
    :tags [] :concepts [] :asset_paths []))

(defun gnosis-edit--render (request)
  "Return REQUEST as pretty-printed JSON text."
  (with-temp-buffer
    (insert (decode-coding-string (gnosis-core--json-body request) 'utf-8-unix))
    (json-pretty-print-buffer)
    (buffer-string)))

(defun gnosis-edit--parse ()
  "Return the JSON request in the current edit buffer."
  (save-excursion
    (goto-char (point-min))
    (condition-case nil
        (let ((value
               (json-parse-buffer
                :object-type 'plist :array-type 'array
                :null-object gnosis-core--json-null
                :false-object gnosis-core--json-false)))
          (skip-chars-forward " \t\r\n")
          (unless (and (eobp) (gnosis-core--json-object-p value))
            (user-error "Thema buffer must contain one JSON object"))
          value)
      (json-parse-error (user-error "Invalid thema JSON")))))

(defun gnosis-edit--replace-request (request)
  "Replace the current buffer with rendered REQUEST."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (insert (gnosis-edit--render request))
    (goto-char (point-min))
    (when (search-forward "\"keimenon\":" nil t)
      (skip-chars-forward " "))
    (set-buffer-modified-p nil)))

(defun gnosis-edit--ensure-buffer-available (name)
  "Reject replacing a modified editor buffer named NAME."
  (when-let* ((buffer (get-buffer name))
              ((buffer-modified-p buffer)))
    (user-error "Gnosis editor has unsaved changes")))

(defun gnosis-edit--open (name request thema-id revision after-save)
  "Open NAME with REQUEST, THEMA-ID, REVISION, and AFTER-SAVE callback."
  (gnosis-edit--ensure-buffer-available name)
  (when (and after-save (not (functionp after-save)))
    (signal 'wrong-type-argument (list 'functionp after-save)))
  (let ((buffer (get-buffer-create name)))
    (pop-to-buffer-same-window buffer)
    (gnosis-edit-mode)
    (setq-local gnosis-edit--thema-id thema-id)
    (setq-local gnosis-edit--revision revision)
    (setq-local gnosis-edit--after-save after-save)
    (setq-local gnosis-edit--write-uncertain nil)
    (gnosis-edit--replace-request request)
    buffer))

;;;###autoload
(defun gnosis-add-thema (&optional after-save)
  "Open a new thema editor and call AFTER-SAVE with its saved projection."
  (interactive)
  (gnosis-edit--ensure-buffer-available gnosis-edit-new-buffer-name)
  (gnosis-edit--open gnosis-edit-new-buffer-name
                     (gnosis-edit--new-request) nil nil after-save))

;;;###autoload
(defun gnosis-edit-thema (&optional thema-id after-save)
  "Edit opaque THEMA-ID and call AFTER-SAVE with its saved projection."
  (interactive (list (read-string "Thema ID: ")))
  (gnosis-edit--ensure-buffer-available gnosis-edit-buffer-name)
  (let* ((thema-id (or thema-id (read-string "Thema ID: ")))
         (projection (gnosis-core-thema thema-id))
         (request (gnosis-core-thema-write-view projection)))
    (gnosis-edit--open gnosis-edit-buffer-name request thema-id
                       (plist-get projection :revision) after-save)))

(defun gnosis-edit--send (request)
  "Send REQUEST once, recording an uncertain mutation outcome."
  (condition-case condition
      (if gnosis-edit--thema-id
          (gnosis-core-thema-update
           gnosis-edit--thema-id gnosis-edit--revision request)
        (gnosis-core-thema-create request))
    (gnosis-core-mutation-uncertain
     (setq gnosis-edit--write-uncertain t)
     (signal (car condition) (cdr condition)))))

(defun gnosis-edit-save ()
  "Save the current thema through the server contract."
  (interactive)
  (unless (derived-mode-p 'gnosis-edit-mode)
    (user-error "Not in a Gnosis editing buffer"))
  (when gnosis-edit--write-uncertain
    (user-error "Mutation outcome is uncertain; cancel and reload before saving"))
  (let* ((request (gnosis-edit--parse))
         (projection (gnosis-edit--send request))
         (saved-request (gnosis-core-thema-write-view projection))
         (thema-id (plist-get projection :thema_id))
         (revision (plist-get projection :revision)))
    (setq gnosis-edit--thema-id thema-id)
    (setq gnosis-edit--revision revision)
    (setq gnosis-edit--write-uncertain nil)
    (gnosis-edit--replace-request saved-request)
    (when gnosis-edit--after-save
      (funcall gnosis-edit--after-save projection))
    (message "Saved thema %s at revision %s" thema-id revision)
    projection))

(defun gnosis-edit-cancel ()
  "Close the current Gnosis editing shell without mutation."
  (interactive)
  (unless (derived-mode-p 'gnosis-edit-mode)
    (user-error "Not in a Gnosis editing buffer"))
  (quit-window t))

(provide 'gnosis-edit)
;;; gnosis-edit.el ends here
