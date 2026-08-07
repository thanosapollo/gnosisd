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

(require 'cl-lib)
(require 'gnosis-core)
(require 'json)
(require 'outline)
(require 'subr-x)

(defconst gnosis-edit--fields
  '((:kind "Kind" scalar)
    (:keimenon "Keimenon" scalar)
    (:hypothesis "Hypothesis" nullable-list)
    (:answer "Answer" list)
    (:source_guid "Source GUID" nullable)
    (:parathema "Parathema" nullable)
    (:review_image "Review image" nullable)
    (:tags "Tags" list)
    (:concepts "Concepts" concepts)
    (:asset_paths "Asset paths" list))
  "Ordered thema request fields and their text representations.")

(defconst gnosis-edit--markers '("@null" "@empty" "@absent")
  "Reserved scalar and list item markers.")

(defconst gnosis-edit--font-lock-keywords
  '(("^\\* .*$" . font-lock-type-face)
    ("^\\*\\* .*$" . font-lock-keyword-face)))

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

(defvar-local gnosis-edit--recovery nil
  "Immutable operation and request retained after unknown delivery.")

(put 'gnosis-edit--recovery 'permanent-local t)

(defvar-local gnosis-edit--authority nil
  "Non-secret authenticated authority that opened this editor.")

(defvar-local gnosis-edit--window-configuration nil
  "Window configuration restored when this editor exits.")

(defvar-keymap gnosis-edit-mode-map
  "C-c C-c" #'gnosis-edit-save
  "C-c C-r" #'gnosis-edit-retry-uncertain
  "C-c C-k" #'gnosis-edit-cancel)

(defun gnosis-edit--allow-kill ()
  "Return non-nil unless this editor retains uncertain recovery."
  (if gnosis-edit--recovery
      (progn
        (message "Gnosis mutation recovery retained; retry or explicitly discard it")
        nil)
    t))

(put 'gnosis-edit--allow-kill 'permanent-local-hook t)

(define-derived-mode gnosis-edit-mode text-mode "Gnosis-Edit"
  "Major mode for editing one Gnosis thema as structured text."
  (add-hook 'kill-buffer-query-functions #'gnosis-edit--allow-kill nil t)
  (setq-local font-lock-defaults '(gnosis-edit--font-lock-keywords)
              outline-regexp "\\*+ "
              header-line-format
              (substitute-command-keys
               " Save \\[gnosis-edit-save], retry \\[gnosis-edit-retry-uncertain], cancel \\[gnosis-edit-cancel]"))
  (outline-minor-mode 1))

(defun gnosis-edit--new-request ()
  "Return a complete initial thema request document."
  `(:kind "basic" :keimenon "" :hypothesis [] :answer []
    :source_guid ,gnosis-core--json-null
    :parathema ,gnosis-core--json-null
    :review_image ,gnosis-core--json-null
    :tags [] :concepts [] :asset_paths []))

(defun gnosis-edit--escape-line (line &optional structural)
  "Escape reserved LINE, including a STRUCTURAL heading when requested."
  (if (or (string-prefix-p "\\" line)
          (member line gnosis-edit--markers)
          (and structural (string-match-p "\\`\\*+ " line)))
      (concat "\\" line)
    line))

(defun gnosis-edit--unescape-line (line &optional structural)
  "Remove one reserved escape from LINE when valid for STRUCTURAL text."
  (if (and (string-prefix-p "\\" line)
           (let ((value (substring line 1)))
             (or (string-prefix-p "\\" value)
                 (member value gnosis-edit--markers)
                 (and structural (string-match-p "\\`\\*+ " value)))))
      (substring line 1)
    line))

(defun gnosis-edit--format-string (value nullable)
  "Return string VALUE encoded as scalar text.
When NULLABLE is non-nil, VALUE may be JSON null."
  (cond
   ((eq value gnosis-core--json-null)
    (if nullable "@null" (user-error "Required thema field cannot be null")))
   ((string-empty-p value) "@empty")
   (t (mapconcat (lambda (line) (gnosis-edit--escape-line line t))
                 (split-string value "\n" nil) "\n"))))

(defun gnosis-edit--format-list-item (value nullable)
  "Return VALUE encoded as one list item, allowing null when NULLABLE."
  (cond
   ((eq value gnosis-core--json-null)
    (if nullable "- @null" (user-error "Thema list item cannot be null")))
   ((string-empty-p value) "- @empty")
   (t
    (let ((lines (split-string value "\n" nil)))
      (concat "- " (gnosis-edit--escape-line (car lines))
              (mapconcat (lambda (line) (concat "\n  |" line))
                         (cdr lines) ""))))))

(defun gnosis-edit--format-list (values nullable)
  "Return vector VALUES as list text, allowing null when NULLABLE."
  (mapconcat (lambda (value) (gnosis-edit--format-list-item value nullable))
             (append values nil) "\n"))

(defun gnosis-edit--format-concepts (concepts)
  "Return ordered CONCEPTS as readable relation and node ID lines."
  (mapconcat
   (lambda (concept)
     (format "- %s %s"
             (plist-get concept :relation)
             (decode-coding-string
              (json-serialize (plist-get concept :node_id)) 'utf-8-unix)))
   (append concepts nil) "\n"))

(defun gnosis-edit--format-value (value type)
  "Return VALUE encoded according to field TYPE."
  (pcase type
    ('scalar (gnosis-edit--format-string value nil))
    ('nullable (gnosis-edit--format-string value t))
    ('list (gnosis-edit--format-list value nil))
    ('nullable-list (gnosis-edit--format-list value t))
    ('concepts (gnosis-edit--format-concepts value))))

(defun gnosis-edit--validate-request-shape (request)
  "Reject REQUEST fields this editor cannot preserve."
  (unless (gnosis-core--json-object-p request)
    (user-error "Invalid thema request object"))
  (let* ((known (mapcar #'car gnosis-edit--fields))
         (unknown
          (cl-loop for tail on request by #'cddr
                   for key = (car tail)
                   unless (memq key known)
                   collect key)))
    (when unknown
      (user-error
       "Unsupported live thema fields: %s"
       (mapconcat (lambda (key) (substring (symbol-name key) 1))
                  unknown ", "))))
  request)

(defun gnosis-edit--format-field (request field)
  "Return one FIELD section encoded from REQUEST."
  (pcase-let ((`(,key ,label ,type) field))
    (format "** %s\n%s" label
            (if (plist-member request key)
                (gnosis-edit--format-value (plist-get request key) type)
              "@absent"))))

(defun gnosis-edit--render (request)
  "Return REQUEST as structured thema text."
  (gnosis-edit--validate-request-shape request)
  (concat "* Thema\n\n"
          (mapconcat (lambda (field)
                       (gnosis-edit--format-field request field))
                     gnosis-edit--fields "\n\n")
          "\n"))

(defun gnosis-edit--section-records (text)
  "Return ordered heading records found in structured thema TEXT."
  (let ((start 0) records)
    (while (string-match "^\\*\\* \\([^\n]+\\)$" text start)
      (push (list (match-string 1 text)
                  (match-beginning 0) (1+ (match-end 0)))
            records)
      (setq start (1+ (match-end 0))))
    (nreverse records)))

(defun gnosis-edit--section-value (text records index)
  "Return section value from TEXT at INDEX in RECORDS."
  (let* ((record (nth index records))
         (next (nth (1+ index) records))
         (start (nth 2 record))
         (end (if next (- (nth 1 next) 2) (1- (length text))))
         (separator (if next
                        (substring text end (nth 1 next))
                      (substring text end))))
    (unless (and (<= start end)
                 (equal separator (if next "\n\n" "\n")))
      (user-error "Malformed thema field boundary"))
    (substring text start end)))

(defun gnosis-edit--sections (text)
  "Return ordered field labels and exact values parsed from TEXT."
  (unless (string-prefix-p "* Thema\n\n** " text)
    (user-error "Thema title is missing"))
  (let* ((records (gnosis-edit--section-records text))
         (labels (mapcar #'car records))
         (expected (mapcar #'cadr gnosis-edit--fields)))
    (unless (equal labels expected)
      (user-error "Thema headings are missing, reordered, or unknown"))
    (cl-loop for index from 0 below (length records)
             collect (cons (nth index labels)
                           (gnosis-edit--section-value text records index)))))

(defun gnosis-edit--parse-string (text nullable)
  "Return scalar TEXT, allowing a null marker when NULLABLE."
  (cond
   ((equal text "@null")
    (if nullable gnosis-core--json-null
      (user-error "Required thema field cannot be null")))
   ((equal text "@empty") "")
   (t (mapconcat (lambda (line) (gnosis-edit--unescape-line line t))
                 (split-string text "\n" nil) "\n"))))

(defun gnosis-edit--parse-list-item (first rest nullable)
  "Decode list item FIRST and REST, allowing null when NULLABLE."
  (cond
   ((member first gnosis-edit--markers)
    (when rest (user-error "Thema marker cannot have continuation lines"))
    (if (equal first "@empty") ""
      (if nullable gnosis-core--json-null
        (user-error "Thema list item cannot be null"))))
   (t (mapconcat #'identity
                 (cons (gnosis-edit--unescape-line first) rest) "\n"))))

(defun gnosis-edit--parse-list (text nullable)
  "Return list vector parsed from TEXT, allowing null when NULLABLE."
  (if (string-empty-p text)
      []
    (let (values first rest started)
      (dolist (line (split-string text "\n" nil))
        (cond
         ((string-match "\\`- \\(.*\\)\\'" line)
          (when started
            (push (gnosis-edit--parse-list-item
                   first (nreverse rest) nullable) values))
          (setq first (match-string 1 line) rest nil started t))
         ((and started (string-match "\\`  |\\(.*\\)\\'" line))
          (push (match-string 1 line) rest))
         (t (user-error "Malformed thema list item: %s" line))))
      (push (gnosis-edit--parse-list-item first (nreverse rest) nullable) values)
      (vconcat (nreverse values)))))

(defun gnosis-edit--parse-concept (line)
  "Return one concept parsed from LINE."
  (unless (string-match
           "\\`- \\(primary\\|supporting\\|reference\\) \\(.+\\)\\'" line)
    (user-error "Malformed thema concept: %s" line))
  (let ((relation (match-string 1 line))
        (encoded (match-string 2 line)))
    (condition-case nil
        (let ((node-id
               (json-parse-string (encode-coding-string encoded 'utf-8-unix))))
          (unless (stringp node-id)
            (user-error "Thema concept node ID must be a string"))
          (list :node_id node-id :relation relation))
      (json-parse-error (user-error "Invalid thema concept node ID")))))

(defun gnosis-edit--parse-concepts (text)
  "Return ordered concept vector parsed from TEXT."
  (if (string-empty-p text) []
    (vconcat (mapcar #'gnosis-edit--parse-concept
                     (split-string text "\n" nil)))))

(defun gnosis-edit--parse-value (text type)
  "Return field TEXT decoded according to TYPE."
  (pcase type
    ('scalar (gnosis-edit--parse-string text nil))
    ('nullable (gnosis-edit--parse-string text t))
    ('list (gnosis-edit--parse-list text nil))
    ('nullable-list (gnosis-edit--parse-list text t))
    ('concepts (gnosis-edit--parse-concepts text))))

(defun gnosis-edit--parse ()
  "Return the thema request in the current edit buffer."
  (let* ((text (buffer-substring-no-properties (point-min) (point-max)))
         (sections (gnosis-edit--sections text)))
    (cl-loop for (key _label type) in gnosis-edit--fields
             for section in sections
             unless (equal (cdr section) "@absent")
             append (list key (gnosis-edit--parse-value (cdr section) type)))))

(defun gnosis-edit--protect-headings ()
  "Make structural headings read-only in the current editor."
  (save-excursion
    (goto-char (point-min))
    (while (re-search-forward "^\\*+ .*$" nil t)
      (add-text-properties
       (line-beginning-position) (min (point-max) (1+ (line-end-position)))
       '(read-only t front-sticky (read-only)
                   rear-nonsticky (read-only))))))

(defun gnosis-edit--replace-request (request)
  "Replace the current buffer with rendered REQUEST."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (insert (gnosis-edit--render request))
    (gnosis-edit--protect-headings)
    (goto-char (point-min))
    (when (re-search-forward "^\\*\\* Keimenon$" nil t)
      (forward-line 1))
    (unless (eq buffer-undo-list t)
      (setq buffer-undo-list nil))
    (set-buffer-modified-p nil)))

(defun gnosis-edit--ensure-buffer-available (name)
  "Reject replacing an occupied editor buffer named NAME."
  (when-let ((buffer (get-buffer name)))
    (when (buffer-local-value 'gnosis-edit--recovery buffer)
      (user-error "Gnosis editor has an uncertain mutation to recover"))
    (when (buffer-modified-p buffer)
      (user-error "Gnosis editor has unsaved changes"))))

(defun gnosis-edit--open (name request thema-id revision authority after-save)
  "Open NAME with REQUEST for THEMA-ID at REVISION under AUTHORITY.
Call AFTER-SAVE with the validated projection after a successful save."
  (gnosis-edit--ensure-buffer-available name)
  (gnosis-edit--validate-request-shape request)
  (when (and after-save (not (functionp after-save)))
    (signal 'wrong-type-argument (list 'functionp after-save)))
  (let ((buffer (get-buffer-create name))
        (window-configuration (current-window-configuration)))
    (pop-to-buffer-same-window buffer)
    (gnosis-edit-mode)
    (setq-local gnosis-edit--thema-id thema-id)
    (setq-local gnosis-edit--revision revision)
    (setq-local gnosis-edit--authority authority)
    (setq-local gnosis-edit--after-save after-save)
    (setq-local gnosis-edit--window-configuration window-configuration)
    (setq-local gnosis-edit--recovery nil)
    (gnosis-edit--replace-request request)
    buffer))

(defun gnosis-edit--request-context ()
  "Return pinned base, token, and authority for one request chain."
  (let* ((base-url (gnosis-core--validated-base-url))
         (token (gnosis-core--token))
         (authority
          (let ((gnosis-core--base-url-override base-url))
            (gnosis-core--authenticated-authority token))))
    (list :base_url base-url :token token :authority authority)))

;;;###autoload
(defun gnosis-add-thema (&optional after-save)
  "Open a new thema editor and call AFTER-SAVE with its saved projection."
  (interactive)
  (gnosis-edit--ensure-buffer-available gnosis-edit-new-buffer-name)
  (let ((context (gnosis-edit--request-context)))
    (gnosis-edit--open gnosis-edit-new-buffer-name
                       (gnosis-edit--new-request) nil nil
                       (plist-get context :authority) after-save)))

;;;###autoload
(defun gnosis-edit-thema (&optional thema-id after-save)
  "Edit opaque THEMA-ID and call AFTER-SAVE with its saved projection."
  (interactive (list (read-string "Thema ID: ")))
  (gnosis-edit--ensure-buffer-available gnosis-edit-buffer-name)
  (let* ((thema-id (or thema-id (read-string "Thema ID: ")))
         (context (gnosis-edit--request-context))
         (base-url (plist-get context :base_url))
         (token (plist-get context :token))
         (authority (plist-get context :authority))
         (projection
          (let ((gnosis-core--base-url-override base-url)
                (gnosis-core--token-override token))
            (gnosis-core-thema thema-id)))
         (request (gnosis-core-thema-write-view projection)))
    (gnosis-edit--open gnosis-edit-buffer-name request thema-id
                       (plist-get projection :revision)
                       authority after-save)))

(defun gnosis-edit--send (operation-id request thema-id revision)
  "Send REQUEST once under OPERATION-ID for THEMA-ID at REVISION.
Record the complete immutable request context after unknown delivery."
  (condition-case condition
      (if thema-id
          (gnosis-core-thema-update
           thema-id operation-id revision request)
        (gnosis-core-thema-create operation-id request))
    (gnosis-core-mutation-uncertain
     (setq gnosis-edit--recovery
           (list :operation-id operation-id
                 :authority gnosis-edit--authority
                 :thema-id thema-id
                 :revision revision
                 :request (copy-tree request t)))
     (signal (car condition) (cdr condition)))))

(defun gnosis-edit--accept (projection)
  "Accept authoritative PROJECTION and return it."
  (let ((saved-request (gnosis-core-thema-write-view projection))
        (thema-id (plist-get projection :thema_id))
        (revision (plist-get projection :revision))
        (after-save gnosis-edit--after-save))
    (setq gnosis-edit--thema-id thema-id
          gnosis-edit--revision revision
          gnosis-edit--recovery nil)
    (gnosis-edit--replace-request saved-request)
    (gnosis-edit--exit)
    (when after-save
      (funcall after-save projection))
    (message "Saved thema %s at revision %s" thema-id revision)
    projection))

(defun gnosis-edit--exit ()
  "Kill current editor and restore its opening window configuration."
  (let ((window-configuration gnosis-edit--window-configuration))
    (quit-window t)
    (when window-configuration
      (set-window-configuration window-configuration))))

(defun gnosis-edit--send-authorized
    (operation-id request thema-id revision)
  "Send REQUEST once under opening authority and immutable OPERATION-ID.
THEMA-ID and REVISION identify an update; both are nil for a create."
  (unless gnosis-edit--authority
    (signal 'gnosis-core-authorization-error
            '("Gnosis editor authority is unavailable")))
  (let* ((context (gnosis-edit--request-context))
         (base-url (plist-get context :base_url))
         (token (plist-get context :token))
         (authority (plist-get context :authority)))
    (unless (equal authority gnosis-edit--authority)
      (signal 'gnosis-core-authorization-error
              '("Gnosis authenticated authority changed")))
    (let ((gnosis-core--base-url-override base-url)
          (gnosis-core--token-override token))
      (gnosis-edit--send operation-id request thema-id revision))))

(defun gnosis-edit-save ()
  "Save the current thema through the server contract."
  (interactive)
  (unless (derived-mode-p 'gnosis-edit-mode)
    (user-error "Not in a Gnosis editing buffer"))
  (when gnosis-edit--recovery
    (user-error "Mutation outcome is uncertain; retry or explicitly discard it"))
  (let* ((request (gnosis-edit--parse))
         (operation-id (gnosis-core-operation-id))
         (projection
          (gnosis-edit--send-authorized
           operation-id request gnosis-edit--thema-id gnosis-edit--revision)))
    (gnosis-edit--accept projection)))

(defun gnosis-edit-retry-uncertain ()
  "Replay the retained uncertain mutation with exact identity and request."
  (interactive)
  (unless gnosis-edit--recovery
    (user-error "No uncertain Gnosis mutation to retry"))
  (let ((operation-id (plist-get gnosis-edit--recovery :operation-id))
        (authority (or (plist-get gnosis-edit--recovery :authority)
                       gnosis-edit--authority))
        (thema-id (plist-get gnosis-edit--recovery :thema-id))
        (revision (plist-get gnosis-edit--recovery :revision))
        (request (plist-get gnosis-edit--recovery :request)))
    (let ((gnosis-edit--authority authority))
      (gnosis-edit--accept
       (gnosis-edit--send-authorized
        operation-id request thema-id revision)))))

(defun gnosis-edit-cancel ()
  "Close the current Gnosis editing shell without mutation."
  (interactive)
  (unless (derived-mode-p 'gnosis-edit-mode)
    (user-error "Not in a Gnosis editing buffer"))
  (when gnosis-edit--recovery
    (user-error "Mutation outcome is uncertain; retry or explicitly discard it"))
  (gnosis-edit--exit))

(defun gnosis-edit-discard-uncertain ()
  "Explicitly discard retained uncertain mutation recovery and close editor."
  (interactive)
  (unless gnosis-edit--recovery
    (user-error "No uncertain Gnosis mutation to discard"))
  (unless (yes-or-no-p "Discard uncertain Gnosis mutation recovery? ")
    (user-error "Gnosis recovery preserved"))
  (setq gnosis-edit--recovery nil)
  (gnosis-edit--exit))

(provide 'gnosis-edit)
;;; gnosis-edit.el ends here
