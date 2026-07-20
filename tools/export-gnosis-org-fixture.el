;;; export-gnosis-org-fixture.el --- Export shared Gnosis Org semantics  -*- lexical-binding: t; -*-

;;; Commentary:

;; Emit the current Elisp projection fields shared with gnosis-core.

;;; Code:

(require 'json)
(require 'org)
(require 'gnosis-org)
(require 'seq)
(require 'subr-x)

(defun gnosis-parity--safe-links ()
  "Return Gnosis ID links while ignoring links without an owner."
  (save-excursion
    (goto-char (point-min))
    (cl-loop while (re-search-forward org-link-any-re nil t)
             for link = (match-string-no-properties 0)
             when (string-match "id:\\([^]]+\\)" link)
             for matched-target = (match-string 1 link)
             for target = (if (string-prefix-p "<id:" link)
                              (string-remove-suffix ">" matched-target)
                            matched-target)
             for owner = (condition-case nil
                             (gnosis-org-get-id)
                           (error nil))
             when (and target owner)
             collect (cons target owner))))

(defun gnosis-parity--local-titles (parsed)
  "Return headline ID and local title pairs from PARSED."
  (org-element-map parsed 'headline
    (lambda (headline)
      (when-let* ((id (org-element-property :ID headline))
                  (raw (org-element-property :raw-value headline)))
        (cons id (gnosis-org-adjust-title (string-trim raw)))))))

(defun gnosis-parity--node (entry file-id topic-title local-titles paths)
  "Return ENTRY's shared JSON data and title path.
FILE-ID and TOPIC-TITLE describe the file node.  LOCAL-TITLES maps
headline IDs to local titles."
  (let* ((id (plist-get entry :id))
         (master (plist-get entry :master))
         (parent-id (unless (equal master 0) master))
         (local-title (if (and file-id (equal id file-id))
                          topic-title
                        (alist-get id local-titles nil nil #'equal)))
         (parent-path (alist-get parent-id paths nil nil #'equal))
         (title-path (append parent-path (list local-title))))
    (cons `((id . ,id)
            (title . ,local-title)
            (title_path . ,(vconcat title-path))
            (tags . ,(vconcat (plist-get entry :tags)))
            (parent_id . ,parent-id)
            (level . ,(plist-get entry :level)))
          title-path)))

(defun gnosis-parity--nodes (entries file-id topic-title local-titles &optional paths)
  "Return normalized ENTRIES using FILE-ID, TOPIC-TITLE, and LOCAL-TITLES."
  (if (null entries)
      []
    (let* ((entry (car entries))
           (node (gnosis-parity--node
                  entry file-id topic-title local-titles paths))
           (next-paths (cons (cons (plist-get entry :id) (cdr node)) paths)))
      (vconcat
       (vector (car node))
       (gnosis-parity--nodes
        (cdr entries) file-id topic-title local-titles next-paths)))))

(defun gnosis-parity--occurrence (link)
  "Convert Gnosis LINK pair to shared occurrence JSON data."
  `((owner . ,(cdr link))
    (target . ,(car link))))

(defun gnosis-parity--edge (link)
  "Convert Gnosis LINK pair to shared semantic-edge JSON data."
  `((source . ,(cdr link))
    (target . ,(car link))))

(defun gnosis-parity--projection ()
  "Return the current buffer's shared Gnosis projection."
  (let* ((parsed (org-element-parse-buffer))
         (topic (gnosis-org-get-data--topic parsed))
         (title (nth 0 topic))
         (tags (nth 1 topic))
         (file-id (nth 2 topic))
         (entries (gnosis-org-buffer-data parsed))
         (links (gnosis-parity--safe-links))
         (edges (seq-uniq links #'equal)))
    `((title . ,title)
      (file_node_id . ,file-id)
      (file_tags . ,(vconcat tags))
      (nodes . ,(gnosis-parity--nodes
                 entries file-id title
                 (gnosis-parity--local-titles parsed)))
      (link_occurrences . ,(vconcat (mapcar #'gnosis-parity--occurrence links)))
      (semantic_edges . ,(vconcat (mapcar #'gnosis-parity--edge edges))))))

(defun gnosis-parity-export-file (filename)
  "Write the shared Gnosis projection for FILENAME to standard output."
  (with-temp-buffer
    (let ((coding-system-for-read 'utf-8-unix))
      (insert-file-contents filename))
    (org-mode)
    (org-set-regexps-and-options 'tags-only)
    (princ (json-encode (gnosis-parity--projection)))))

(let ((filename (or (getenv "GNOSIS_PARITY_FIXTURE")
                    (car command-line-args-left))))
  (unless filename
    (error "Usage: export-gnosis-org-fixture.el FILE"))
  (gnosis-parity-export-file filename))

;;; export-gnosis-org-fixture.el ends here
