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

(defconst gnosis-edit--kinds '("basic" "cloze" "mcq" "mc-cloze")
  "Thema kinds supported by the authoring mode.")

(defconst gnosis-edit--max-review-cloze-inputs 500
  "Maximum cloze inputs accepted by the daemon.")

(defconst gnosis-edit--metadata-fields
  '((:source_guid "Source GUID" optional)
    (:parathema "Parathema" optional)
    (:review_image "Review image" optional)
    (:tags "Tags" list)
    (:concepts "Concepts" concepts)
    (:asset_paths "Asset paths" list))
  "Ordered secondary fields in a Thema authoring buffer.")

(defconst gnosis-edit--supported-keys
  '(:kind :keimenon :hypothesis :answer :source_guid :parathema
    :review_image :tags :concepts :asset_paths)
  "Top-level request keys preserved by the authoring mode.")

(defconst gnosis-edit--markers '("[none]" "[empty]")
  "Reserved readable value markers.")

(defconst gnosis-edit--font-lock-keywords
  '(("^\\* .*$" . font-lock-type-face)
    ("^\\*\\* .*$" . font-lock-keyword-face)
    ("{{\\|}}" . font-lock-builtin-face)
    ("^- \\[x\\] " . font-lock-constant-face)))

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

(defvar-local gnosis-edit--original-request nil
  "Request from which the current authoring buffer was rendered.")

(defvar-keymap gnosis-edit-mode-map
  "TAB" #'gnosis-edit-tab
  "RET" #'gnosis-edit-activate
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
               " RET \\[gnosis-edit-activate], fold \\[gnosis-edit-tab], save \\[gnosis-edit-save], cancel \\[gnosis-edit-cancel]"))
  (outline-minor-mode 1))

(defun gnosis-edit-tab ()
  "Fold a heading or indent ordinary authoring text at point."
  (interactive)
  (if (save-excursion
        (beginning-of-line)
        (looking-at-p outline-regexp))
      (outline-toggle-children)
    (indent-for-tab-command)))

(defun gnosis-edit--stateful-field-p (label)
  "Return non-nil when LABEL supports an explicit value state."
  (member label '("Hint" "Source GUID" "Parathema" "Review image"
                  "Tags" "Concepts" "Asset paths")))

(defun gnosis-edit--collection-field-p (label)
  "Return non-nil when LABEL names a collection field."
  (member label '("Tags" "Concepts" "Asset paths")))

(defun gnosis-edit--state-choice (label current)
  "Read a new state for LABEL, defaulting to CURRENT."
  (let ((choices (if (gnosis-edit--collection-field-p label)
                     '("values" "omitted")
                   '("value" "none" "empty" "omitted"))))
    (completing-read (format "%s state: " label) choices nil t nil nil
                     (if (member current choices) current (car choices)))))

(defun gnosis-edit--choice-heading (label choice)
  "Return a heading for LABEL in selected state CHOICE."
  (format "** %s%s" label
          (pcase choice
            ((or "value" "values") "")
            (state (format " [%s]" state)))))

(defun gnosis-edit--clear-section-value (editable)
  "Clear the current heading value and leave room when EDITABLE."
  (let ((start (line-beginning-position 2))
        (next (save-excursion
                (forward-line 1)
                (when (re-search-forward "^\\*\\* " nil t)
                  (line-beginning-position)))))
    (if next
        (progn
          (delete-region start next)
          (goto-char start)
          (insert (if editable "\n\n" "\n")))
      (delete-region start (point-max))
      (when editable (insert "\n")))))

(defun gnosis-edit--set-field-state (label choice current)
  "Set heading LABEL to CHOICE from CURRENT state and position its value."
  (let ((heading (copy-marker (line-beginning-position)))
        (inhibit-read-only t)
        (clear (or (not (member choice '("value" "values")))
                   (not (member current '("value" "values"))))))
    (delete-region (line-beginning-position) (line-end-position))
    (insert (gnosis-edit--choice-heading label choice))
    (when clear
      (gnosis-edit--clear-section-value
       (member choice '("value" "values"))))
    (gnosis-edit--protect-headings)
    (goto-char heading)
    (set-marker heading nil)
    (forward-line 1)))

(defun gnosis-edit--activate-field-heading (heading)
  "Activate state-bearing field HEADING at point."
  (let* ((label (gnosis-edit--label-base heading))
         (state (gnosis-edit--label-state heading label))
         (current (if (eq state 'value)
                      (if (gnosis-edit--collection-field-p label)
                          "values" "value")
                    (symbol-name state)))
         (choice (gnosis-edit--state-choice label current)))
    (gnosis-edit--set-field-state label choice current)))

(defun gnosis-edit--current-section ()
  "Return the current secondary heading label and body bounds."
  (save-excursion
    (beginning-of-line)
    (when (re-search-backward "^\\*\\* \\(.+\\)$" nil t)
      (let ((label (gnosis-edit--label-base
                    (substring-no-properties (match-string 1))))
            (start (line-beginning-position 2)))
        (goto-char start)
        (list label start
              (or (and (re-search-forward "^\\*+ " nil t)
                       (line-beginning-position))
                  (point-max)))))))

(defun gnosis-edit--select-choice-at-point ()
  "Select the choice item at point and clear the previous selection."
  (let* ((target (copy-marker (line-beginning-position)))
         (section (gnosis-edit--current-section)))
    (save-excursion
      (goto-char (nth 1 section))
      (while (re-search-forward "^- \\[x\\] " (nth 2 section) t)
        (replace-match "- [ ] " t t)))
    (goto-char target)
    (when (looking-at "- \\[ \\] ")
      (replace-match "- [x] " t t))
    (set-marker target nil)))

(defun gnosis-edit-activate ()
  "Edit a field state, select a choice, or insert a newline at point."
  (interactive)
  (cond
   ((and (equal (car (gnosis-edit--current-section)) "Choices")
         (save-excursion (beginning-of-line) (looking-at "- \\[[ x]\\] ")))
    (gnosis-edit--select-choice-at-point))
   ((save-excursion (beginning-of-line) (looking-at "\\*\\* \\(.+\\)$"))
    (beginning-of-line)
    (let* ((heading (substring-no-properties (match-string 1)))
           (label (gnosis-edit--label-base heading)))
      (if (gnosis-edit--stateful-field-p label)
          (gnosis-edit--activate-field-heading heading)
        (forward-line 1))))
   (t (newline))))

(defun gnosis-edit--new-request (&optional kind)
  "Return an initial request document for optional KIND."
  (let ((kind (or kind "basic")))
    (unless (member kind gnosis-edit--kinds)
      (user-error "Unsupported Thema kind"))
    `(:kind ,kind :keimenon "" :hypothesis [] :answer []
    :source_guid ,gnosis-core--json-null
    :parathema ,gnosis-core--json-null
    :review_image ,gnosis-core--json-null
      :tags [] :concepts [] :asset_paths [])))

(defun gnosis-edit-read-kind ()
  "Read a Thema kind for a new authoring buffer."
  (completing-read "Thema kind: " gnosis-edit--kinds nil t nil nil "basic"))

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

(defun gnosis-edit--format-string (value)
  "Return string VALUE encoded as authoring text."
  (unless (stringp value)
    (user-error "Invalid Thema text value"))
  (mapconcat (lambda (line) (gnosis-edit--escape-line line t))
             (split-string value "\n" nil) "\n"))

(defun gnosis-edit--format-list-item (value &optional prefix)
  "Return string VALUE as a list item following optional PREFIX."
  (unless (stringp value)
    (user-error "Invalid Thema list value"))
  (let* ((lines (split-string value "\n" nil))
         (first (if (string-empty-p value)
                    "[empty]"
                  (gnosis-edit--escape-line (car lines))))
         (prefix (or prefix "- ")))
    (concat prefix first
            (mapconcat
             (lambda (line)
               (concat "\n  |" (gnosis-edit--escape-line line)))
             (cdr lines) ""))))

(defun gnosis-edit--format-list (values)
  "Return vector VALUES as readable list text."
  (unless (vectorp values)
    (user-error "Invalid Thema list"))
  (mapconcat #'gnosis-edit--format-list-item (append values nil) "\n"))

(defun gnosis-edit--format-concepts (concepts)
  "Return ordered CONCEPTS as readable relation and node ID lines."
  (mapconcat
   (lambda (concept)
     (format "- %s %s"
             (plist-get concept :relation)
             (decode-coding-string
              (json-serialize (plist-get concept :node_id)) 'utf-8-unix)))
   (append concepts nil) "\n"))

(defun gnosis-edit--unsupported-keys (object supported)
  "Return keys in OBJECT that are not members of SUPPORTED."
  (cl-loop for tail on object by #'cddr
           for key = (car tail)
           unless (memq key supported)
           collect key))

(defun gnosis-edit--validate-request-shape (request)
  "Reject REQUEST fields this editor cannot preserve."
  (unless (gnosis-core--json-object-p request)
    (user-error "Invalid thema request object"))
  (let ((unknown (gnosis-edit--unsupported-keys
                  request gnosis-edit--supported-keys)))
    (when unknown
      (user-error
       "Unsupported live thema fields: %s"
       (mapconcat (lambda (key) (substring (symbol-name key) 1))
                  unknown ", "))))
  (when (plist-member request :concepts)
    (unless (vectorp (plist-get request :concepts))
      (user-error "Invalid thema concepts"))
    (cl-loop for concept across (plist-get request :concepts)
             for object-p = (gnosis-core--json-object-p concept)
             for unknown = (and object-p
                                (gnosis-edit--unsupported-keys
                                 concept '(:node_id :relation)))
             unless object-p
             do (user-error "Invalid thema concept object")
             when unknown
             do (user-error "Unsupported live thema concept fields: %s"
                            (mapconcat
                             (lambda (key) (substring (symbol-name key) 1))
                             unknown ", "))))
  request)

(defun gnosis-edit--format-state-section (label state &optional value)
  "Return a LABEL section with STATE and optional string VALUE."
  (let ((heading (pcase state
                   ('omitted (format "** %s [omitted]" label))
                   ('none (format "** %s [none]" label))
                   ('empty (format "** %s [empty]" label))
                   (_ (format "** %s" label)))))
    (if (eq state 'value)
        (format "%s\n%s" heading (gnosis-edit--format-string value))
      heading)))

(defun gnosis-edit--format-section (label value)
  "Return a LABEL heading followed by VALUE when it is nonempty."
  (if (string-empty-p value)
      (format "** %s" label)
    (format "** %s\n%s" label value)))

(defun gnosis-edit--format-optional (request key label)
  "Return optional REQUEST field KEY as a readable LABEL section."
  (if (not (plist-member request key))
      (gnosis-edit--format-state-section label 'omitted)
    (let ((value (plist-get request key)))
      (cond
       ((eq value gnosis-core--json-null)
        (gnosis-edit--format-state-section label 'none))
       ((and (stringp value) (string-empty-p value))
        (gnosis-edit--format-state-section label 'empty))
       ((stringp value)
        (gnosis-edit--format-state-section label 'value value))
       (t (user-error "Invalid optional Thema field: %s" label))))))

(defun gnosis-edit--format-collection (request key label formatter)
  "Return collection KEY from REQUEST under LABEL using FORMATTER."
  (if (not (plist-member request key))
      (gnosis-edit--format-state-section label 'omitted)
    (gnosis-edit--format-section
     label (funcall formatter (plist-get request key)))))

(defun gnosis-edit--format-metadata (request)
  "Return secondary REQUEST fields as authoring sections."
  (mapcar
   (lambda (field)
     (pcase-let ((`(,key ,label ,type) field))
       (pcase type
         ('optional (gnosis-edit--format-optional request key label))
         ('list (gnosis-edit--format-collection
                 request key label #'gnosis-edit--format-list))
         ('concepts (gnosis-edit--format-collection
                     request key label #'gnosis-edit--format-concepts)))))
   gnosis-edit--metadata-fields))

(defun gnosis-edit--format-hint (request)
  "Return a basic Thema hint section from REQUEST."
  (if (not (plist-member request :hypothesis))
      (gnosis-edit--format-state-section "Hint" 'omitted)
    (let ((values (plist-get request :hypothesis)))
      (unless (and (vectorp values) (<= (length values) 1))
        (user-error "Basic Thema accepts at most one hint"))
      (if (= (length values) 0)
          "** Hint"
        (let ((value (aref values 0)))
          (cond
           ((eq value gnosis-core--json-null)
            (gnosis-edit--format-state-section "Hint" 'none))
           ((and (stringp value) (string-empty-p value))
            (gnosis-edit--format-state-section "Hint" 'empty))
           ((stringp value)
            (gnosis-edit--format-state-section "Hint" 'value value))
           (t (user-error "Invalid basic Thema hint"))))))))

(defun gnosis-edit--format-basic (request)
  "Return primary authoring sections for basic REQUEST."
  (let ((answers (plist-get request :answer)))
    (unless (and (vectorp answers) (<= (length answers) 1))
      (user-error "Basic Thema accepts one answer"))
    (list (gnosis-edit--format-section
           "Question" (gnosis-edit--format-string
                       (or (plist-get request :keimenon) "")))
          (gnosis-edit--format-hint request)
          (gnosis-edit--format-section
           "Answer" (if (= (length answers) 0) ""
                      (gnosis-edit--format-string (aref answers 0)))))))

(defun gnosis-edit--range-overlap-p (start end spans)
  "Return non-nil when START..END overlaps a range in SPANS."
  (cl-some (lambda (span)
             (and (< start (cadr span)) (> end (car span))))
           spans))

(defun gnosis-edit--trim-outer-quotes (value)
  "Strip one trailing and one leading quote from VALUE."
  (let ((value (if (string-suffix-p "\"" value)
                   (substring value 0 -1)
                 value)))
    (if (string-prefix-p "\"" value) (substring value 1) value)))

(defun gnosis-edit--unicode-whitespace-p (character)
  "Return non-nil when CHARACTER is whitespace according to Rust."
  (or (and (<= #x9 character) (<= character #xd))
      (memq character
            '(#x20 #x85 #xa0 #x1680 #x2028 #x2029 #x202f #x205f #x3000))
      (and (<= #x2000 character) (<= character #x200a))))

(defun gnosis-edit--unicode-trim-bounds (value)
  "Return Rust-compatible Unicode trim bounds for VALUE."
  (let ((start 0)
        (end (length value)))
    (while (and (< start end)
                (gnosis-edit--unicode-whitespace-p (aref value start)))
      (setq start (1+ start)))
    (while (and (> end start)
                (gnosis-edit--unicode-whitespace-p (aref value (1- end))))
      (setq end (1- end)))
    (cons start end)))

(defun gnosis-edit--answer-range (text answer spans)
  "Return the first unused folded ANSWER range in TEXT outside SPANS."
  (let ((start 0) found (case-fold-search t))
    (while (and (not found)
                (string-match (regexp-quote answer) text start))
      (let ((begin (match-beginning 0)) (end (match-end 0)))
        (if (gnosis-edit--range-overlap-p begin end spans)
            (setq start end)
          (setq found (list begin end)))))
    found))

(defun gnosis-edit--cloze-spans (text answers)
  "Return daemon-compatible TEXT spans for vector ANSWERS."
  (unless (vectorp answers)
    (user-error "Invalid cloze answers"))
  (let (spans)
    (cl-loop for answer across answers
             for index from 0
             for matched = (and (stringp answer)
                                (gnosis-edit--trim-outer-quotes answer))
             for trim-bounds = (and matched
                                    (gnosis-edit--unicode-trim-bounds matched))
             for range = (and matched
                              (< (car trim-bounds) (cdr trim-bounds))
                              (gnosis-edit--answer-range text matched spans))
             unless (and matched
                         (< (car trim-bounds) (cdr trim-bounds)))
             do (user-error "Cloze answers cannot be empty")
             unless range
             do (user-error "Cloze answer is not text in the question: %s"
                            answer)
             do (push (append range (list index)) spans))
    (sort spans (lambda (left right) (< (car left) (car right))))))

(defun gnosis-edit--escape-cloze-text (text)
  "Escape literal cloze delimiters in TEXT."
  (mapconcat
   (lambda (line)
     (if (string-match-p "\\`\\*+ " line) (concat "\\" line) line))
   (split-string
    (string-replace "{{" "\\{{" (string-replace "\\" "\\\\" text))
    "\n" nil)
   "\n"))

(defun gnosis-edit--escape-cloze-value (text)
  "Escape TEXT used inside one cloze marker."
  (let ((escaped (string-replace "\\" "\\\\" text)))
    (setq escaped (string-replace "}}" "\\}}" escaped))
    (setq escaped (string-replace " :: " " \\:: " escaped))
    (setq escaped (string-replace " => " " \\=> " escaped))
    (setq escaped
          (mapconcat
           (lambda (line)
             (if (string-match-p "\\`\\*+ " line) (concat "\\" line) line))
           (split-string escaped "\n" nil) "\n"))
    (if (member escaped gnosis-edit--markers) (concat "\\" escaped) escaped)))

(defun gnosis-edit--cloze-answer-suffix (source answer)
  "Return a marker suffix preserving ANSWER when it differs from SOURCE."
  (if (equal source answer) ""
    (concat " => " (gnosis-edit--escape-cloze-value answer))))

(defun gnosis-edit--cloze-hint-suffix (request index)
  "Return inline hint suffix for REQUEST answer at INDEX."
  (let ((hints (and (plist-member request :hypothesis)
                    (plist-get request :hypothesis))))
    (cond
     ((or (not (vectorp hints)) (>= index (length hints))) "")
     ((eq (aref hints index) gnosis-core--json-null) " :: [none]")
     ((equal (aref hints index) "") " :: [empty]")
     ((stringp (aref hints index))
      (concat " :: " (gnosis-edit--escape-cloze-value (aref hints index))))
     (t (user-error "Invalid cloze hint")))))

(defun gnosis-edit--review-cloze-hint (request index)
  "Return daemon-equivalent concealed hint for REQUEST answer INDEX."
  (let* ((hints (plist-get request :hypothesis))
         (hint (and (vectorp hints) (< index (length hints))
                    (aref hints index))))
    (if (and (stringp hint) (not (member hint '("" "nil" "\"\""))))
        hint
      "...")))

(defun gnosis-edit--concealed-cloze-text (request spans)
  "Return daemon-equivalent concealed text for REQUEST using SPANS."
  (let ((text (plist-get request :keimenon))
        (answers (plist-get request :answer))
        (cursor 0)
        parts)
    (dolist (span spans)
      (let* ((answer (gnosis-edit--trim-outer-quotes
                      (aref answers (nth 2 span))))
             (trim-bounds (gnosis-edit--unicode-trim-bounds answer)))
        (push (substring text cursor (car span)) parts)
        (push (concat (substring answer 0 (car trim-bounds))
                      "(" (gnosis-edit--review-cloze-hint
                           request (nth 2 span)) ")"
                      (substring answer (cdr trim-bounds)))
              parts))
      (setq cursor (cadr span)))
    (push (substring text cursor) parts)
    (apply #'concat (nreverse parts))))

(defun gnosis-edit--validate-cloze-review-safety (request spans)
  "Reject REQUEST when concealed text from SPANS exposes an answer."
  (let ((question (gnosis-edit--concealed-cloze-text request spans))
        (case-fold-search t))
    (cl-loop for answer across (plist-get request :answer)
             for untrimmed = (gnosis-edit--trim-outer-quotes answer)
             for trim-bounds = (gnosis-edit--unicode-trim-bounds untrimmed)
             for matched = (substring untrimmed
                                      (car trim-bounds) (cdr trim-bounds))
             when (string-match-p (regexp-quote matched) question)
             do (user-error "Cloze answer remains visible in the question: %s"
                            answer))))

(defun gnosis-edit--format-cloze-text (request)
  "Return REQUEST question with semantic cloze markers."
  (let* ((text (or (plist-get request :keimenon) ""))
         (answers (or (plist-get request :answer) []))
         (spans (if (= (length answers) 0) nil
                  (gnosis-edit--cloze-spans text answers)))
         (cursor 0) parts)
    (when spans
      (gnosis-edit--validate-cloze-review-safety request spans))
    (dolist (span spans)
      (let ((source (substring text (car span) (cadr span)))
            (answer (aref answers (nth 2 span))))
        (push (gnosis-edit--escape-cloze-text
               (substring text cursor (car span))) parts)
        (push (concat "{{" (gnosis-edit--escape-cloze-value source)
                      (gnosis-edit--cloze-answer-suffix source answer)
                      (gnosis-edit--cloze-hint-suffix request (nth 2 span))
                      "}}") parts))
      (setq cursor (cadr span)))
    (push (gnosis-edit--escape-cloze-text (substring text cursor)) parts)
    (apply #'concat (nreverse parts))))

(defun gnosis-edit--format-cloze (request)
  "Return the semantic cloze section for REQUEST."
  (list (gnosis-edit--format-section
         "Text" (gnosis-edit--format-cloze-text request))))

(defun gnosis-edit--format-choices (request)
  "Return choice items with REQUEST answer selected."
  (let ((choices (or (plist-get request :hypothesis) []))
        (answers (or (plist-get request :answer) []))
        selected)
    (unless (and (vectorp choices) (vectorp answers) (<= (length answers) 1))
      (user-error "Invalid choice Thema"))
    (mapconcat
     (lambda (choice)
       (let ((checked (and (not selected) (= (length answers) 1)
                           (equal choice (aref answers 0)))))
         (when checked (setq selected t))
         (gnosis-edit--format-list-item
          choice (if checked "- [x] " "- [ ] "))))
     (append choices nil) "\n")))

(defun gnosis-edit--format-choice (request)
  "Return primary authoring sections for a choice REQUEST."
  (list (gnosis-edit--format-section
         "Question" (gnosis-edit--format-string
                     (or (plist-get request :keimenon) "")))
        (gnosis-edit--format-section
         "Choices" (gnosis-edit--format-choices request))))

(defun gnosis-edit--primary-sections (request)
  "Return type-aware primary sections for REQUEST."
  (pcase (plist-get request :kind)
    ("basic" (gnosis-edit--format-basic request))
    ("cloze" (gnosis-edit--format-cloze request))
    ((or "mcq" "mc-cloze") (gnosis-edit--format-choice request))
    (_ (user-error "Unsupported Thema kind"))))

(defun gnosis-edit--render (request)
  "Return REQUEST as semantic authoring text."
  (gnosis-edit--validate-request-shape request)
  (let ((kind (plist-get request :kind)))
    (unless (member kind gnosis-edit--kinds)
      (user-error "Unsupported Thema kind"))
    (concat "* Thema [" kind "]\n\n"
            (mapconcat #'identity (gnosis-edit--primary-sections request)
                       "\n\n")
            "\n\n* Metadata\n\n"
            (mapconcat #'identity (gnosis-edit--format-metadata request)
                       "\n\n")
            "\n")))

(defun gnosis-edit--section-records (text)
  "Return ordered level-1/level-2 heading records from thema TEXT.
Deeper outline stars are body text, not structural section boundaries."
  (let ((start 0) records)
    (while (string-match "^\\(\\*\\{1,2\\}\\) \\([^\n]+\\)$" text start)
      (push (list (length (match-string 1 text))
                  (match-string 2 text)
                  (match-beginning 0) (1+ (match-end 0)))
            records)
      (setq start (1+ (match-end 0))))
    (nreverse records)))

(defun gnosis-edit--section-value (text records index)
  "Return section value from TEXT at INDEX in RECORDS."
  (let* ((record (nth index records))
         (next (nth (1+ index) records))
         (start (nth 3 record))
         (end (if next (nth 2 next) (length text)))
         (segment (substring text start end)))
    (cond
     ((and next (equal segment "\n")) "")
     ((and next (string-suffix-p "\n\n" segment))
      (substring segment 0 -2))
     ((and (not next) (string-empty-p segment)) "")
     ((and (not next) (string-suffix-p "\n" segment))
      (substring segment 0 -1))
     (t (user-error "Malformed thema field boundary")))))

(defun gnosis-edit--sections (text)
  "Return ordered field labels and exact values parsed from TEXT."
  (unless (string-match
           "\\`\\* Thema \\[\\(basic\\|cloze\\|mcq\\|mc-cloze\\)\\]\n\n\\*\\* "
           text)
    (user-error "Thema title is missing"))
  (let* ((kind (match-string 1 text))
         (records (gnosis-edit--section-records text))
         (top-level (cl-loop for (level label) in records
                             when (= level 1) collect label)))
    (unless (equal top-level (list (format "Thema [%s]" kind) "Metadata"))
      (user-error "Thema groups are missing, reordered, or unknown"))
    ;; Group headings carry no body; only the blank separator is allowed.
    (cl-loop for record in records
             for index from 0
             when (= (car record) 1)
             do (unless (string-empty-p
                         (gnosis-edit--section-value text records index))
                  (user-error "Malformed thema group prologue")))
    (cons kind
          (cl-loop for record in records
                   for index from 0
                   when (= (car record) 2)
                   collect (cons (nth 1 record)
                                 (gnosis-edit--section-value
                                  text records index))))))

(defun gnosis-edit--parse-string (text)
  "Return authoring scalar TEXT."
  (mapconcat (lambda (line) (gnosis-edit--unescape-line line t))
             (split-string text "\n" nil) "\n"))

(defun gnosis-edit--parse-list-item (first rest)
  "Decode list item FIRST and continuation lines REST."
  (if (equal first "[empty]")
      (progn
        (when rest (user-error "Empty item cannot have continuation lines"))
        "")
    (mapconcat #'identity
               (cons (gnosis-edit--unescape-line first)
                     (mapcar #'gnosis-edit--unescape-line rest))
               "\n")))

(defun gnosis-edit--parse-list (text)
  "Return list vector parsed from authoring TEXT."
  (if (string-empty-p text)
      []
    (let (values first rest started)
      (dolist (line (split-string text "\n" nil))
        (cond
         ((string-match "\\`- \\(.*\\)\\'" line)
          (when started
            (push (gnosis-edit--parse-list-item
                   first (nreverse rest)) values))
          (setq first (match-string 1 line) rest nil started t))
         ((and started (string-match "\\`  |\\(.*\\)\\'" line))
          (push (match-string 1 line) rest))
         (t (user-error "Malformed thema list item: %s" line))))
      (push (gnosis-edit--parse-list-item first (nreverse rest)) values)
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

(defun gnosis-edit--label-base (label)
  "Return field name from state-bearing LABEL."
  (if (string-match "\\`\\(.+\\) \\[\\(omitted\\|none\\|empty\\)\\]\\'" label)
      (match-string 1 label)
    label))

(defun gnosis-edit--label-state (label base)
  "Return state encoded by LABEL for field BASE."
  (cond
   ((equal label base) 'value)
   ((equal label (format "%s [omitted]" base)) 'omitted)
   ((equal label (format "%s [none]" base)) 'none)
   ((equal label (format "%s [empty]" base)) 'empty)
   (t (user-error "Invalid state for Thema field: %s" base))))

(defun gnosis-edit--expected-labels (kind)
  "Return ordered base labels for KIND."
  (append (pcase kind
            ("basic" '("Question" "Hint" "Answer"))
            ("cloze" '("Text"))
            ((or "mcq" "mc-cloze") '("Question" "Choices")))
          (mapcar #'cadr gnosis-edit--metadata-fields)))

(defun gnosis-edit--validate-section-order (kind sections)
  "Require SECTIONS to contain exactly the ordered fields for KIND."
  (unless (equal (mapcar (lambda (section)
                           (gnosis-edit--label-base (car section)))
                         sections)
                 (gnosis-edit--expected-labels kind))
    (user-error "Thema headings are missing, reordered, or unknown")))

(defun gnosis-edit--unescaped-p (text position)
  "Return non-nil when TEXT at POSITION is not backslash escaped."
  (let ((cursor (1- position)) (slashes 0))
    (while (and (>= cursor 0) (= (aref text cursor) ?\\))
      (setq slashes (1+ slashes) cursor (1- cursor)))
    (= (% slashes 2) 0)))

(defun gnosis-edit--find-unescaped (token text start)
  "Return next unescaped TOKEN position in TEXT at or after START."
  (let ((cursor start) found)
    (while (and (not found)
                (string-match (regexp-quote token) text cursor))
      (if (gnosis-edit--unescaped-p text (match-beginning 0))
          (setq found (match-beginning 0))
        (setq cursor (match-end 0))))
    found))

(defun gnosis-edit--unescape-cloze (text)
  "Remove one quoting backslash from cloze TEXT."
  (let ((cursor 0) characters)
    (while (< cursor (length text))
      (if (and (= (aref text cursor) ?\\)
               (< (1+ cursor) (length text)))
          (progn
            (push (aref text (1+ cursor)) characters)
            (setq cursor (+ cursor 2)))
        (push (aref text cursor) characters)
        (setq cursor (1+ cursor))))
    (concat (nreverse characters))))

(defun gnosis-edit--parse-cloze-marker (text)
  "Return source, answer, hint-presence, and hint from marker TEXT."
  (let* ((hint-separator (gnosis-edit--find-unescaped " :: " text 0))
         (body (if hint-separator (substring text 0 hint-separator) text))
         (answer-separator (gnosis-edit--find-unescaped " => " body 0))
         (source (gnosis-edit--unescape-cloze
                  (if answer-separator
                      (substring body 0 answer-separator)
                    body)))
         (answer (if answer-separator
                     (gnosis-edit--unescape-cloze
                      (substring body (+ answer-separator 4)))
                   source))
         (raw-hint (and hint-separator
                        (substring text (+ hint-separator 4))))
         (hint (cond ((equal raw-hint "[none]") gnosis-core--json-null)
                     ((equal raw-hint "[empty]") "")
                     (raw-hint (gnosis-edit--unescape-cloze raw-hint)))))
    (list source answer (not (null hint-separator)) hint)))

(defun gnosis-edit--hint-vector (records)
  "Return a hypothesis vector from ordered hint RECORDS."
  (let ((last (cl-loop for record in records for index from 0
                       when (nth 2 record) maximize index)))
    (if (null last) []
      (vconcat
       (cl-loop for record in records for index from 0 to last
                collect (if (nth 2 record)
                            (nth 3 record)
                          gnosis-core--json-null))))))

(defun gnosis-edit--restore-cloze-order (records)
  "Return RECORDS in the original request's answer order when possible."
  (let* ((request gnosis-edit--original-request)
         (answers (and (equal (plist-get request :kind) "cloze")
                       (plist-get request :answer)))
         (text (and request (plist-get request :keimenon)))
         (spans (and (stringp text) (vectorp answers)
                     (= (length answers) (length records))
                     (gnosis-edit--cloze-spans text answers))))
    (if (not spans) records
      (mapcar
       #'cdr
       (sort (cl-mapcar (lambda (span record)
                          (cons (nth 2 span) record))
                        spans records)
             (lambda (left right) (< (car left) (car right))))))))

(defun gnosis-edit--parse-cloze-text (text)
  "Return keimenon, answers, and hints derived from cloze TEXT."
  (let ((cursor 0) pieces records
        (start (gnosis-edit--find-unescaped "{{" text 0)))
    (while start
      (let ((end (gnosis-edit--find-unescaped "}}" text (+ start 2))))
        (unless end (user-error "Unclosed cloze marker"))
        (let* ((marker (gnosis-edit--parse-cloze-marker
                        (substring text (+ start 2) end)))
               (source (car marker))
               (answer (nth 1 marker)))
          (when (or (string-empty-p source) (string-empty-p answer))
            (user-error "Empty cloze answer"))
          (push (gnosis-edit--unescape-cloze
                 (substring text cursor start)) pieces)
          (push source pieces)
          (push marker records)
          (setq cursor (+ end 2)
                start (gnosis-edit--find-unescaped "{{" text cursor)))))
    (push (gnosis-edit--unescape-cloze (substring text cursor)) pieces)
    (setq records (gnosis-edit--restore-cloze-order (nreverse records)))
    (list :keimenon (apply #'concat (nreverse pieces))
          :hypothesis (gnosis-edit--hint-vector records)
          :answer (vconcat (mapcar (lambda (record) (nth 1 record))
                                   records)))))

(defun gnosis-edit--parse-basic-hint (section)
  "Return a hypothesis field list parsed from basic hint SECTION."
  (let ((state (gnosis-edit--label-state (car section) "Hint")))
    (pcase state
      ('omitted nil)
      ('none (list :hypothesis (vector gnosis-core--json-null)))
      ('empty (list :hypothesis [""]))
      (_ (list :hypothesis
               (if (string-empty-p (cdr section)) []
                 (vector (gnosis-edit--parse-string (cdr section)))))))))

(defun gnosis-edit--parse-basic (sections)
  "Return primary request fields parsed from basic SECTIONS."
  (let ((question (nth 0 sections)) (hint (nth 1 sections))
        (answer (nth 2 sections)))
    (append (list :keimenon (gnosis-edit--parse-string (cdr question)))
            (gnosis-edit--parse-basic-hint hint)
            (list :answer
                  (if (string-empty-p (cdr answer)) []
                    (vector (gnosis-edit--parse-string (cdr answer))))))))

(defun gnosis-edit--parse-choice-item (first rest checked)
  "Return choice FIRST plus REST and whether it is CHECKED."
  (list (gnosis-edit--parse-list-item first (nreverse rest)) checked))

(defun gnosis-edit--plist-delete (plist key)
  "Return PLIST without KEY."
  (cl-loop for (field value) on plist by #'cddr
           unless (eq field key)
           append (list field value)))

(defun gnosis-edit--parse-choices (text)
  "Return choices and selected answer parsed from TEXT."
  (let (records first rest checked started)
    (dolist (line (split-string text "\n" nil))
      (cond
       ((string-match "\\`- \\[\\([ x]\\)\\] \\(.*\\)\\'" line)
        (when started
          (push (gnosis-edit--parse-choice-item first rest checked) records))
        (setq checked (equal (match-string 1 line) "x")
              first (match-string 2 line) rest nil started t))
       ((and started (string-match "\\`  |\\(.*\\)\\'" line))
        (push (match-string 1 line) rest))
       (t (user-error "Malformed Thema choice: %s" line))))
    (when started
      (push (gnosis-edit--parse-choice-item first rest checked) records))
    (setq records (nreverse records))
    (let ((selected (cl-loop for record in records
                             when (cadr record) collect (car record))))
      (when (> (length selected) 1)
        (user-error "Select only one Thema answer"))
      (list :hypothesis (vconcat (mapcar #'car records))
            :answer (vconcat selected)))))

(defun gnosis-edit--parse-primary (kind sections)
  "Return fields for KIND parsed from SECTIONS."
  (pcase kind
    ("basic" (gnosis-edit--parse-basic sections))
    ("cloze"
     (let ((fields (gnosis-edit--parse-cloze-text (cdar sections))))
       (if (or (plist-member gnosis-edit--original-request :hypothesis)
               (> (length (plist-get fields :hypothesis)) 0))
           fields
         (gnosis-edit--plist-delete fields :hypothesis))))
    ((or "mcq" "mc-cloze")
     (append (list :keimenon (gnosis-edit--parse-string (cdar sections)))
             (gnosis-edit--parse-choices (cdr (nth 1 sections)))))))

(defun gnosis-edit--parse-optional (section key label)
  "Return optional KEY parsed from SECTION named LABEL."
  (pcase (gnosis-edit--label-state (car section) label)
    ('omitted nil)
    ('none (list key gnosis-core--json-null))
    ('empty (list key ""))
    (_ (list key (gnosis-edit--parse-string (cdr section))))))

(defun gnosis-edit--parse-metadata-field (section field)
  "Return one metadata FIELD parsed from SECTION."
  (pcase-let ((`(,key ,label ,type) field))
    (if (eq (gnosis-edit--label-state (car section) label) 'omitted)
        nil
      (pcase type
        ('optional (gnosis-edit--parse-optional section key label))
        ('list (list key (gnosis-edit--parse-list (cdr section))))
        ('concepts (list key (gnosis-edit--parse-concepts (cdr section))))))))

(defun gnosis-edit--parse-metadata (sections)
  "Return metadata fields parsed from ordered SECTIONS."
  (cl-loop for section in sections
           for field in gnosis-edit--metadata-fields
           append (gnosis-edit--parse-metadata-field section field)))

(defun gnosis-edit--validate-draft (request)
  "Return REQUEST when its kind-specific authoring shape is valid."
  (let ((kind (plist-get request :kind))
        (question (plist-get request :keimenon))
        (answers (plist-get request :answer))
        (hints (and (plist-member request :hypothesis)
                    (plist-get request :hypothesis))))
    (unless (and (stringp question) (not (string-empty-p (string-trim question)))
                 (vectorp answers)
                 (> (length answers) 0)
                 (cl-every (lambda (value)
                             (and (stringp value)
                                  (not (string-empty-p (string-trim value)))))
                           (append answers nil)))
      (user-error "Thema question and answers cannot be blank"))
    (pcase kind
      ("basic"
       (unless (and (= (length answers) 1)
                    (or (null hints)
                        (and (vectorp hints) (<= (length hints) 1))))
         (user-error "Basic Thema requires one answer and at most one hint")))
      ("cloze"
       (unless (and (<= (length answers)
                        gnosis-edit--max-review-cloze-inputs)
                    (or (null hints)
                        (and (vectorp hints) (<= (length hints) (length answers))))
                    (gnosis-edit--cloze-spans question answers))
         (user-error "Invalid cloze Thema")))
      ((or "mcq" "mc-cloze")
       (unless (and (vectorp hints) (>= (length hints) 2)
                    (= (length answers) 1)
                    (member (aref answers 0) (append hints nil)))
         (user-error "Choice Thema requires choices and one selected answer"))))
    request))

(defun gnosis-edit--parse (&optional allow-incomplete)
  "Return the request in the current authoring buffer.
When ALLOW-INCOMPLETE is non-nil, return an unfinished new draft."
  (let* ((text (buffer-substring-no-properties (point-min) (point-max)))
         (parsed (gnosis-edit--sections text))
         (kind (car parsed))
         (sections (cdr parsed))
         (primary-count (pcase kind
                          ("basic" 3)
                          ("cloze" 1)
                          (_ 2)))
         (primary (cl-subseq sections 0 primary-count))
         (metadata (nthcdr primary-count sections)))
    (gnosis-edit--validate-section-order kind sections)
    (let ((request (append (list :kind kind)
                           (gnosis-edit--parse-primary kind primary)
                           (gnosis-edit--parse-metadata metadata))))
      (gnosis-edit--validate-request-shape request)
      (if allow-incomplete request (gnosis-edit--validate-draft request)))))

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
    (setq gnosis-edit--original-request (copy-tree request t))
    (insert (gnosis-edit--render request))
    (gnosis-edit--protect-headings)
    (save-excursion
      (goto-char (point-min))
      (when (re-search-forward "^\\* Metadata$" nil t)
        (outline-hide-subtree)))
    (goto-char (point-min))
    (when (re-search-forward "^\\*\\* \\(Question\\|Text\\)$" nil t)
      (forward-line 1))
    (unless (eq buffer-undo-list t)
      (setq buffer-undo-list nil))
    (set-buffer-modified-p nil)))

(defun gnosis-edit--ensure-buffer-available (name)
  "Reject replacing an occupied editor buffer named NAME."
  (when-let* ((buffer (get-buffer name)))
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
(defun gnosis-add-thema (&optional after-save kind)
  "Open a new KIND editor and call AFTER-SAVE with its saved projection."
  (interactive (list nil (gnosis-edit-read-kind)))
  (gnosis-edit--ensure-buffer-available gnosis-edit-new-buffer-name)
  (let ((context (gnosis-edit--request-context)))
    (gnosis-edit--open gnosis-edit-new-buffer-name
                       (gnosis-edit--new-request kind) nil nil
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
            (gnosis-core-thema thema-id))))
    (when (eq t (plist-get projection :archived))
      (user-error "Deleted thema cannot be edited"))
    (let ((request (gnosis-core-thema-write-view projection)))
      (gnosis-edit--open gnosis-edit-buffer-name request thema-id
                         (plist-get projection :revision)
                         authority after-save))))

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
