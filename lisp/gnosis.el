;;; gnosis.el --- Emacs client foundation for Gnosis  -*- lexical-binding: t; -*-

;; Copyright (C) 2022-2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; URL: https://thanosapollo.org/projects/gnosis
;; Version: 0.13.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: extensions, learning

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Gnosis provides the authenticated HTTP, OpenAPI, and presentation
;; foundation for the gnosisd service.  Domain commands remain unavailable
;; until generated bindings target the stable public contract.

;;; Code:

(require 'gnosis-core)
(require 'org)

(defgroup gnosis nil
  "Emacs client for Gnosis."
  :group 'applications
  :prefix "gnosis-")

(defface gnosis-face-correct
  '((t :inherit success))
  "Face reserved for successful review content."
  :group 'gnosis)

(defface gnosis-face-false
  '((t :inherit error))
  "Face reserved for unsuccessful review content."
  :group 'gnosis)

(defface gnosis-face-hint
  '((t :inherit warning))
  "Face for Gnosis hints and metadata."
  :group 'gnosis)

(defface gnosis-face-separator
  '((((background light)) :strike-through "gray70" :foreground "gray70")
    (t :strike-through "gray30" :foreground "gray30"))
  "Face reserved for review separators."
  :group 'gnosis)

(defface gnosis-face-directions
  '((t :inherit underline))
  "Face reserved for review labels."
  :group 'gnosis)

(defface gnosis-face-next-review
  '((t :inherit bold))
  "Face reserved for the next review date."
  :group 'gnosis)

(defface gnosis-face-dashboard-header
  '((t :inherit (bold font-lock-constant-face)))
  "Face for the Gnosis foundation dashboard header."
  :group 'gnosis)

(defcustom gnosis-center-content-during-review t
  "Non-nil means center content in future review buffers."
  :type 'boolean
  :group 'gnosis)

(defcustom gnosis-latex-preview nil
  "Non-nil means render LaTeX fragments in Gnosis content."
  :type 'boolean
  :group 'gnosis)

(defvar gnosis-dashboard-buffer-name "*Gnosis Dashboard*"
  "Foundation dashboard buffer name.")

(defvar-local gnosis-center-content t
  "Non-nil means center content in the current Gnosis buffer.")

(defvar-keymap gnosis-dashboard-mode-map
  "g" #'gnosis-dashboard-refresh
  "q" #'quit-window)

(define-derived-mode gnosis-dashboard-mode special-mode "Gnosis"
  "Major mode for the Gnosis foundation dashboard."
  (setq-local truncate-lines nil
              revert-buffer-function
              (lambda (&rest _) (gnosis-dashboard-refresh))))

(defun gnosis--display-link-descriptions (string)
  "Return STRING with bracket links replaced by their descriptions."
  (replace-regexp-in-string
   "\\[\\[[^]]+\\]\\[\\([^]]+\\)\\]\\]" "\\1" (or string "") t))

(defun gnosis--materialize-display-overlays ()
  "Copy display overlays in the current buffer to text properties."
  (dolist (overlay (overlays-in (point-min) (point-max)))
    (when-let* ((display (overlay-get overlay 'display)))
      (put-text-property (overlay-start overlay) (overlay-end overlay)
                         'display display))
    (delete-overlay overlay)))

(defun gnosis-org-format-string (string)
  "Return STRING fontified for display like Org content."
  (with-temp-buffer
    (org-mode)
    (setq-local org-pretty-entities t)
    (insert (gnosis--display-link-descriptions string))
    (font-lock-ensure)
    (when gnosis-latex-preview
      (org-latex-preview '(16))
      (gnosis--materialize-display-overlays))
    (buffer-string)))

(defun gnosis-center-string (string)
  "Return STRING centered for the selected window width."
  (let* ((width (max 1 (window-body-width)))
         (padding (max 0 (/ (- width (string-width string)) 2))))
    (concat (make-string padding ?\s) string)))

(defun gnosis-format-string (string)
  "Return STRING prepared for the current Gnosis presentation buffer."
  (let ((formatted (gnosis-org-format-string string)))
    (if gnosis-center-content
        (mapconcat #'gnosis-center-string (split-string formatted "\n") "\n")
      formatted)))

(defun gnosis--foundation-state ()
  "Return validated daemon health and OpenAPI values."
  (list :health (gnosis-core-status) :openapi (gnosis-core-openapi)))

(defun gnosis--openapi-path-count (document)
  "Return number of paths in OpenAPI DOCUMENT."
  (/ (length (plist-get document :paths)) 2))

(defun gnosis--render-foundation (state)
  "Render foundation STATE in the current dashboard buffer."
  (let* ((health (plist-get state :health))
         (openapi (plist-get state :openapi))
         (inhibit-read-only t))
    (erase-buffer)
    (insert (propertize "Gnosis" 'face 'gnosis-face-dashboard-header)
            "\n\nDaemon: " (plist-get health :status)
            "\nOpenAPI: " (plist-get openapi :openapi)
            "\nPaths: "
            (number-to-string (gnosis--openapi-path-count openapi))
            "\n\nDomain commands are not yet available.\n"
            "Bindings will be generated after the public API stabilizes.\n")
    (goto-char (point-min))))

(defun gnosis-dashboard-refresh ()
  "Refresh the current Gnosis foundation dashboard."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-mode)
    (user-error "Not in a Gnosis dashboard"))
  (gnosis--render-foundation (gnosis--foundation-state)))

(defun gnosis-dashboard ()
  "Open the Gnosis foundation dashboard."
  (interactive)
  (let ((state (gnosis--foundation-state))
        (buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (gnosis-dashboard-mode)
    (gnosis--render-foundation state)))

;;;###autoload
(defun gnosis ()
  "Open the Gnosis foundation dashboard."
  (interactive)
  (gnosis-dashboard))

(provide 'gnosis)
;;; gnosis.el ends here
